use std::{
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use parking_lot::Mutex;
use rusqlite::params;
use tracing_forest::tree::Tree;
use tracing_subscriber::{Layer, filter::LevelFilter, fmt::MakeWriter, layer::SubscriberExt};

use super::engine::ProgressItem;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TraceFormat {
    Forest,
    Flat,
}

fn trace_settings(trace: u8) -> anyhow::Result<Option<(TraceFormat, LevelFilter)>> {
    Ok(match trace {
        0 => None,
        1 => Some((TraceFormat::Forest, LevelFilter::INFO)),
        2 => Some((TraceFormat::Forest, LevelFilter::DEBUG)),
        3 => Some((TraceFormat::Flat, LevelFilter::DEBUG)),
        4 => Some((TraceFormat::Flat, LevelFilter::TRACE)),
        _ => anyhow::bail!("trace level must be between zero and four"),
    })
}

pub fn override_thread_subscriber(
    db_path: impl AsRef<Path>,
    trace: u8,
    progress: Option<ProgressItem>,
    reverse_lines: bool,
) -> anyhow::Result<(tracing::subscriber::DefaultGuard, Arc<AtomicU32>)> {
    let settings = trace_settings(trace)?;
    let current_id = Arc::new(AtomicU32::default());
    let (forest_output, flat_progress) = match (settings, progress) {
        (Some((TraceFormat::Forest, level)), Some(progress)) => (
            ForestOutput::Progress {
                progress: Mutex::new(progress),
                level,
            },
            None,
        ),
        (Some((TraceFormat::Forest, level)), None) => (ForestOutput::Stderr(level), None),
        (Some((TraceFormat::Flat, _)), progress) => (ForestOutput::None, progress),
        (None, _) => (ForestOutput::None, None),
    };
    let processor = tracing_forest::Printer::new()
        .writer(tracing_forest::printer::MakeStderr)
        .formatter(StoreTreeToDb {
            con: Arc::new(Mutex::new(rusqlite::Connection::open(&db_path)?)),
            run_id: current_id.clone(),
            output: forest_output,
            reverse_lines,
        });
    let forest = tracing_forest::ForestLayer::from(processor);
    let guard = match settings {
        None | Some((TraceFormat::Forest, _)) => {
            tracing::subscriber::set_default(tracing_subscriber::registry().with(forest))
        }
        Some((TraceFormat::Flat, level)) => match flat_progress {
            Some(progress) => tracing::subscriber::set_default(
                tracing_subscriber::registry().with(forest).with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(ProgressWriter(Mutex::new(progress)))
                        .with_filter(level),
                ),
            ),
            None => tracing::subscriber::set_default(
                tracing_subscriber::registry().with(forest).with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_filter(level),
                ),
            ),
        },
    };
    Ok((guard, current_id))
}

struct ProgressWriter(Mutex<ProgressItem>);

impl<'a> MakeWriter<'a> for ProgressWriter {
    type Writer = io::LineWriter<ProgressWriteGuard<'a>>;

    fn make_writer(&'a self) -> Self::Writer {
        io::LineWriter::new(ProgressWriteGuard(self.0.lock()))
    }
}

struct ProgressWriteGuard<'a>(parking_lot::MutexGuard<'a, ProgressItem>);

impl io::Write for ProgressWriteGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use gix::Progress;
        for line in buf.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            self.0.info(String::from_utf8_lossy(line).into_owned());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum ForestOutput {
    None,
    Progress {
        progress: Mutex<ProgressItem>,
        level: LevelFilter,
    },
    Stderr(LevelFilter),
}

pub struct StoreTreeToDb {
    con: Arc<Mutex<rusqlite::Connection>>,
    run_id: Arc<AtomicU32>,
    output: ForestOutput,
    reverse_lines: bool,
}

impl tracing_forest::printer::Formatter for StoreTreeToDb {
    type Error = rusqlite::Error;

    fn fmt(&self, tree: &Tree) -> Result<String, Self::Error> {
        let rendered = match &self.output {
            ForestOutput::None => None,
            ForestOutput::Progress { progress, level } => {
                if let Some(tree) = filtered_forest(tree, *level) {
                    use gix::Progress;
                    let progress = &mut progress.lock();
                    if self.reverse_lines {
                        for line in tree.lines().rev() {
                            progress.info(line.into());
                        }
                    } else {
                        for line in tree.lines() {
                            progress.info(line.into());
                        }
                    }
                }
                None
            }
            ForestOutput::Stderr(level) => filtered_forest(tree, *level),
        };
        // TODO: wait for new release of `tracing-forest` and load the ID from span fields.
        let json = serde_json::to_string_pretty(&tree).expect("serialization to string always works");
        let run_id = self.run_id.load(Ordering::SeqCst);
        self.con
            .lock()
            .execute("UPDATE run SET spans_json = ?1 WHERE id = ?2", params![json, run_id])?;
        Ok(rendered.unwrap_or_default())
    }
}

fn filtered_forest(tree: &Tree, level: LevelFilter) -> Option<String> {
    use tracing_forest::Formatter;

    let tree = tracing_forest::printer::Pretty.fmt(tree).ok()?;
    let mut out = String::new();
    // ponytail: filter Pretty's stable level prefix until tracing-forest can host two forest layers safely.
    for line in tree.lines().filter(|line| match level {
        LevelFilter::INFO => !line.starts_with("DEBUG") && !line.starts_with("TRACE"),
        LevelFilter::DEBUG => !line.starts_with("TRACE"),
        _ => true,
    }) {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gix::progress::prodash::tree::Root;

    use super::{TraceFormat, trace_settings};
    use crate::corpus::{db, engine::ProgressItem};
    use tracing_subscriber::filter::LevelFilter;

    #[test]
    fn trace_repetitions_choose_format_and_level() -> anyhow::Result<()> {
        assert_eq!(trace_settings(0)?, None);
        assert_eq!(trace_settings(1)?, Some((TraceFormat::Forest, LevelFilter::INFO)));
        assert_eq!(trace_settings(2)?, Some((TraceFormat::Forest, LevelFilter::DEBUG)));
        assert_eq!(trace_settings(3)?, Some((TraceFormat::Flat, LevelFilter::DEBUG)));
        assert_eq!(trace_settings(4)?, Some((TraceFormat::Flat, LevelFilter::TRACE)));
        assert_eq!(
            trace_settings(5)
                .expect_err("trace output has only four levels")
                .to_string(),
            "trace level must be between zero and four"
        );
        Ok(())
    }

    #[test]
    fn requested_trace_mode_controls_progress_format_and_level() -> anyhow::Result<()> {
        let fixture = tempfile::tempdir()?;

        let forest_info = messages(fixture.path(), 1)?;
        assert_eq!(forest_info.len(), 2);
        assert!(forest_info.iter().all(|line| line.starts_with("INFO")));

        let forest_debug = messages(fixture.path(), 2)?;
        assert_eq!(forest_debug.len(), 3);
        assert!(forest_debug.iter().any(|line| line.starts_with("DEBUG")));

        let flat_debug = messages(fixture.path(), 3)?;
        assert_eq!(flat_debug.len(), 2);
        assert!(flat_debug.iter().any(|line| line.contains(" DEBUG ")));
        assert!(!flat_debug.iter().any(|line| line.contains(" TRACE ")));

        let flat_trace = messages(fixture.path(), 4)?;
        assert_eq!(flat_trace.len(), 3);
        assert!(flat_trace.iter().any(|line| line.contains(" TRACE ")));
        Ok(())
    }

    #[test]
    fn forest_output_does_not_filter_the_stored_trace() -> anyhow::Result<()> {
        let fixture = tempfile::tempdir()?;
        let db_path = fixture.path().join("stored.db");
        let connection = db::create(&db_path)?;
        connection.execute("INSERT INTO run (insertion_time) VALUES (0)", [])?;
        let run_id = u32::try_from(connection.last_insert_rowid()).expect("test run id fits in u32");
        drop(connection);

        {
            let progress = Root::new();
            let item = ProgressItem::from(Some(progress.add_child("test")));
            let (_guard, current_id) = super::override_thread_subscriber(&db_path, 1, Some(item), false)?;
            current_id.store(run_id, std::sync::atomic::Ordering::SeqCst);
            tracing::info_span!("root").in_scope(|| tracing::debug!("stored debug event"));
        }

        let connection = rusqlite::Connection::open(db_path)?;
        let stored: String =
            connection.query_row("SELECT spans_json FROM run WHERE id = ?1", [run_id], |row| row.get(0))?;
        assert!(
            stored.contains("stored debug event"),
            "display filtering leaves storage complete"
        );
        Ok(())
    }

    fn messages(root: &Path, trace: u8) -> anyhow::Result<Vec<String>> {
        let db_path = root.join(format!("trace-{trace}.db"));
        drop(db::create(&db_path)?);
        let progress = Root::new();
        let item = ProgressItem::from(Some(progress.add_child("test")));
        {
            let (_guard, _current_id) = super::override_thread_subscriber(&db_path, trace, Some(item), false)?;
            tracing::info_span!("root").in_scope(|| {
                tracing::info!("info event");
                tracing::debug!("debug event");
                tracing::trace!("trace event");
            });
        }
        let mut messages = Vec::new();
        progress.copy_messages(&mut messages);
        Ok(messages.into_iter().map(|message| message.message).collect())
    }
}
