use bstr::{BStr, BString};

use crate::{
    extension::{FsMonitor, Signature},
    util::{read_u32, read_u64, split_at_byte_exclusive},
};

#[derive(Clone)]
pub enum Token {
    V1 { nanos_since_1970: u64 },
    V2 { token: BString },
}

pub const SIGNATURE: Signature = *b"FSMN";

impl FsMonitor {
    /// Create a version-two filesystem monitor extension with an opaque `token` and dirty-entry bitmap.
    ///
    /// Return `None` if the token contains a NUL byte or the bitmap cannot represent the entry count.
    pub fn from_token(token: impl Into<BString>, dirty_entries: &[bool]) -> Option<Self> {
        let token = token.into();
        if token.contains(&0) {
            return None;
        }
        Some(Self {
            token: Token::V2 { token },
            entry_dirty: gix_bitmap::ewah::Vec::from_bits(dirty_entries)?,
        })
    }

    /// Return the opaque version-two filesystem monitor token, or `None` for a legacy timestamp token.
    pub fn token(&self) -> Option<&BStr> {
        match &self.token {
            Token::V1 { .. } => None,
            Token::V2 { token } => Some(token.as_ref()),
        }
    }

    /// Call `f` for each index entry which must not be trusted without checking the worktree.
    ///
    /// Return `None` if the bitmap is malformed or if `f` requests early termination.
    pub fn for_each_dirty_entry(&self, f: impl FnMut(usize) -> Option<()>) -> Option<()> {
        self.entry_dirty.for_each_set_bit(f)
    }

    pub(crate) fn write_to(&self, mut out: impl std::io::Write) -> std::io::Result<()> {
        let mut bitmap = Vec::new();
        self.entry_dirty.write_to(&mut bitmap)?;
        let bitmap_size = u32::try_from(bitmap.len())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "fsmonitor bitmap exceeds 4GB"))?;

        let mut payload = Vec::new();
        match &self.token {
            Token::V1 { nanos_since_1970 } => {
                payload.extend_from_slice(&1_u32.to_be_bytes());
                payload.extend_from_slice(&nanos_since_1970.to_be_bytes());
            }
            Token::V2 { token } => {
                payload.extend_from_slice(&2_u32.to_be_bytes());
                payload.extend_from_slice(token);
                payload.push(0);
            }
        }
        payload.extend_from_slice(&bitmap_size.to_be_bytes());
        payload.extend_from_slice(&bitmap);

        let payload_size = u32::try_from(payload.len())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "fsmonitor extension exceeds 4GB"))?;
        out.write_all(&SIGNATURE)?;
        out.write_all(&payload_size.to_be_bytes())?;
        out.write_all(&payload)
    }
}

pub fn decode(data: &[u8]) -> Option<FsMonitor> {
    let (version, data) = read_u32(data)?;
    let (token, data) = match version {
        1 => {
            let (nanos_since_1970, data) = read_u64(data)?;
            (Token::V1 { nanos_since_1970 }, data)
        }
        2 => {
            let (token, data) = split_at_byte_exclusive(data, 0)?;
            (Token::V2 { token: token.into() }, data)
        }
        _ => return None,
    };

    let (ewah_size, data) = read_u32(data)?;
    let ((entry_dirty, extra), data) = data
        .split_at_checked(ewah_size as usize)
        .and_then(|(entry_dirty, data)| {
            gix_bitmap::ewah::decode(entry_dirty)
                .ok()
                .map(|entry_dirty| (entry_dirty, data))
        })?;
    if !extra.is_empty() {
        return None;
    }

    if !data.is_empty() {
        return None;
    }

    FsMonitor { token, entry_dirty }.into()
}
