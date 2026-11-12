use bstr::BString;
use gix_hash::ObjectId;

use crate::{
    entry,
    extension::{Signature, UntrackedCache},
    util::{read_u32, split_at_byte_exclusive, var_int},
};

impl UntrackedCache {
    /// Something identifying the location and machine that this cache is for.
    pub fn identifier(&self) -> &bstr::BStr {
        self.identifier.as_ref()
    }

    /// Stat and object id for the `.git/info/exclude` file, if available.
    pub fn info_exclude(&self) -> Option<&OidStat> {
        self.info_exclude.as_ref()
    }

    /// Stat and object id for the `core.excludesfile`, if available.
    pub fn excludes_file(&self) -> Option<&OidStat> {
        self.excludes_file.as_ref()
    }

    /// Usually `.gitignore`.
    pub fn exclude_filename_per_dir(&self) -> &bstr::BStr {
        self.exclude_filename_per_dir.as_ref()
    }

    /// The directory flags Git used while populating the cache.
    pub fn dir_flags(&self) -> u32 {
        self.dir_flags
    }

    /// A list of directories and sub-directories, with `directories[0]` being the root.
    pub fn directories(&self) -> &[Directory] {
        &self.directories
    }

    pub(crate) fn write_to(&self, mut out: impl std::io::Write, object_hash: gix_hash::Kind) -> std::io::Result<()> {
        let mut payload = Vec::new();
        write_var_int(&mut payload, self.identifier.len())?;
        payload.extend_from_slice(&self.identifier);

        write_stat(&mut payload, self.info_exclude.as_ref().map(|entry| &entry.stat))?;
        write_stat(&mut payload, self.excludes_file.as_ref().map(|entry| &entry.stat))?;
        payload.extend_from_slice(&self.dir_flags.to_be_bytes());
        write_object_id(
            &mut payload,
            self.info_exclude.as_ref().map(|entry| entry.id),
            object_hash,
        )?;
        write_object_id(
            &mut payload,
            self.excludes_file.as_ref().map(|entry| entry.id),
            object_hash,
        )?;
        payload.extend_from_slice(&self.exclude_filename_per_dir);
        payload.push(0);

        write_var_int(&mut payload, self.directories.len())?;
        if !self.directories.is_empty() {
            self.write_directory(&mut payload, 0)?;

            let valid: Vec<bool> = self
                .directories
                .iter()
                .map(|directory| directory.stat.is_some())
                .collect();
            let check_only: Vec<bool> = self.directories.iter().map(|directory| directory.check_only).collect();
            let hash_valid: Vec<bool> = self
                .directories
                .iter()
                .map(|directory| directory.exclude_file_oid.is_some())
                .collect();
            for bitmap in [&valid, &check_only, &hash_valid] {
                gix_bitmap::ewah::Vec::from_bits(bitmap)
                    .ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::InvalidInput, "untracked-cache bitmap exceeds 4GB")
                    })?
                    .write_to(&mut payload)?;
            }

            for directory in &self.directories {
                if let Some(stat) = directory.stat.as_ref() {
                    write_stat(&mut payload, Some(stat))?;
                }
            }
            for directory in &self.directories {
                if let Some(object_id) = directory.exclude_file_oid {
                    write_object_id(&mut payload, Some(object_id), object_hash)?;
                }
            }
            payload.push(0);
        }

        let payload_size = u32::try_from(payload.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "untracked-cache extension exceeds 4GB",
            )
        })?;
        out.write_all(&SIGNATURE)?;
        out.write_all(&payload_size.to_be_bytes())?;
        out.write_all(&payload)
    }

    fn write_directory(&self, out: &mut Vec<u8>, index: usize) -> std::io::Result<()> {
        let directory = self.directories.get(index).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "untracked-cache directory references an invalid child",
            )
        })?;
        write_var_int(&mut *out, directory.untracked_entries.len())?;
        write_var_int(&mut *out, directory.sub_directories.len())?;
        out.extend_from_slice(&directory.name);
        out.push(0);
        for entry in &directory.untracked_entries {
            out.extend_from_slice(entry);
            out.push(0);
        }
        for &child in &directory.sub_directories {
            self.write_directory(out, child)?;
        }
        Ok(())
    }
}

fn write_var_int(mut out: impl std::io::Write, value: usize) -> std::io::Result<()> {
    let mut value = u64::try_from(value)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "untracked-cache count exceeds u64"))?;
    let mut encoded = [0_u8; 10];
    let mut position = encoded.len() - 1;
    encoded[position] = (value & 0x7f) as u8;
    while {
        value >>= 7;
        value != 0
    } {
        value -= 1;
        position -= 1;
        encoded[position] = 0x80 | (value & 0x7f) as u8;
    }
    out.write_all(&encoded[position..])
}

fn write_stat(mut out: impl std::io::Write, stat: Option<&entry::Stat>) -> std::io::Result<()> {
    let stat = stat.copied().unwrap_or_default();
    for value in [
        stat.ctime.secs,
        stat.ctime.nsecs,
        stat.mtime.secs,
        stat.mtime.nsecs,
        stat.dev,
        stat.ino,
        stat.uid,
        stat.gid,
        stat.size,
    ] {
        out.write_all(&value.to_be_bytes())?;
    }
    Ok(())
}

fn write_object_id(
    mut out: impl std::io::Write,
    object_id: Option<ObjectId>,
    object_hash: gix_hash::Kind,
) -> std::io::Result<()> {
    out.write_all(object_id.unwrap_or_else(|| ObjectId::null(object_hash)).as_bytes())
}

/// A structure to track filesystem stat information along with an object id, linking a worktree file with what's in our ODB.
#[derive(Clone, Debug)]
pub struct OidStat {
    /// The file system stat information
    pub stat: entry::Stat,
    /// The id of the file in our ODB.
    pub id: ObjectId,
}

impl OidStat {
    /// The file system stat information.
    pub fn stat(&self) -> &entry::Stat {
        &self.stat
    }

    /// The id of the file in our ODB.
    pub fn id(&self) -> ObjectId {
        self.id
    }
}

/// A directory with information about its untracked files, and its sub-directories
#[derive(Clone, Debug)]
pub struct Directory {
    /// The directories name, or an empty string if this is the root directory.
    pub name: BString,
    /// Untracked files and directory names
    pub untracked_entries: Vec<BString>,
    /// indices for sub-directories similar to this one.
    pub sub_directories: Vec<usize>,

    /// The directories stat data, if available or valid // TODO: or is it the exclude file?
    pub stat: Option<entry::Stat>,
    /// The oid of a .gitignore file, if it exists
    pub exclude_file_oid: Option<ObjectId>,
    /// TODO: figure out what this really does
    pub check_only: bool,
}

impl Directory {
    /// The directory name, or an empty string if this is the root directory.
    /// `/` is always used as path-separator.
    pub fn name(&self) -> &bstr::BStr {
        self.name.as_ref()
    }

    /// Untracked files and directory names.
    pub fn untracked_entries(&self) -> &[BString] {
        &self.untracked_entries
    }

    /// Indices for sub-directories similar to this one.
    pub fn sub_directories(&self) -> &[usize] {
        &self.sub_directories
    }

    /// The directory stat data, if available and valid.
    pub fn stat(&self) -> Option<&entry::Stat> {
        self.stat.as_ref()
    }

    /// The oid of a `.gitignore` file, if it exists.
    pub fn exclude_file_oid(&self) -> Option<ObjectId> {
        self.exclude_file_oid
    }

    /// Whether Git marked this directory as check-only.
    pub fn check_only(&self) -> bool {
        self.check_only
    }
}

/// Only used as an indicator
pub const SIGNATURE: Signature = *b"UNTR";

/// Decode an untracked cache extension from `data`, assuming object hashes are of type `object_hash`.
pub fn decode(data: &[u8], object_hash: gix_hash::Kind, alloc_limit_bytes: Option<usize>) -> Option<UntrackedCache> {
    if data.last().is_none_or(|b| *b != 0) {
        return None;
    }
    let (identifier_len, data) = var_int(data)?;
    let (identifier, data) = data.split_at_checked(identifier_len.try_into().ok()?)?;

    // The on-disk layout matches git's `ondisk_untracked_cache` struct
    // https://github.com/git/git/blob/2855562ca6a9c6b0e7bc780b050c1e83c9fcfbd0/dir.c#L3582-L3586
    // https://github.com/git/git/blob/2855562ca6a9c6b0e7bc780b050c1e83c9fcfbd0/dir.c#L3668-L3722
    //   info_exclude_stat  (36 bytes)
    //   excludes_file_stat (36 bytes)
    //   dir_flags          ( 4 bytes)
    //   info_exclude hash  (hash_len bytes)
    //   excludes_file hash (hash_len bytes)
    //   exclude_per_dir    (NUL-terminated)
    let hash_len = object_hash.len_in_bytes();
    let (info_exclude_stat, data) = crate::decode::stat(data)?;
    let (excludes_file_stat, data) = crate::decode::stat(data)?;
    let (dir_flags, data) = read_u32(data)?;
    let (info_exclude_hash, data) = data.split_at_checked(hash_len)?;
    let (excludes_file_hash, data) = data.split_at_checked(hash_len)?;
    let info_exclude = OidStat {
        stat: info_exclude_stat,
        id: ObjectId::from_bytes_or_panic(info_exclude_hash),
    };
    let excludes_file = OidStat {
        stat: excludes_file_stat,
        id: ObjectId::from_bytes_or_panic(excludes_file_hash),
    };
    let (exclude_filename_per_dir, data) = split_at_byte_exclusive(data, 0)?;

    let (num_directory_blocks, data) = var_int(data)?;

    let mut res = UntrackedCache {
        identifier: identifier.into(),
        info_exclude: (!info_exclude.id.is_null()).then_some(info_exclude),
        excludes_file: (!excludes_file.id.is_null()).then_some(excludes_file),
        exclude_filename_per_dir: exclude_filename_per_dir.into(),
        dir_flags,
        directories: Vec::new(),
    };
    if num_directory_blocks == 0 {
        return data.is_empty().then_some(res);
    }

    let num_directory_blocks: usize = num_directory_blocks.try_into().ok()?;
    if num_directory_blocks > data.len() {
        return None;
    }
    if alloc_limit_bytes
        .is_some_and(|limit| num_directory_blocks.saturating_mul(std::mem::size_of::<Directory>()) > limit)
    {
        return None;
    }
    let directories = &mut res.directories;
    directories.try_reserve(num_directory_blocks).ok()?;

    let data = decode_directory_block(data, directories, alloc_limit_bytes)?;
    if directories.len() != num_directory_blocks {
        return None;
    }
    let (valid, data) = gix_bitmap::ewah::decode(data).ok()?;
    let (check_only, data) = gix_bitmap::ewah::decode(data).ok()?;
    let (hash_valid, mut data) = gix_bitmap::ewah::decode(data).ok()?;

    if valid.num_bits() > num_directory_blocks
        || check_only.num_bits() > num_directory_blocks
        || hash_valid.num_bits() > num_directory_blocks
    {
        return None;
    }

    check_only.for_each_set_bit(|index| {
        let directory = directories.get_mut(index)?;
        directory.check_only = true;
        Some(())
    })?;
    valid.for_each_set_bit(|index| {
        let directory = directories.get_mut(index)?;
        let (stat, rest) = crate::decode::stat(data)?;
        directory.stat = stat.into();
        data = rest;
        Some(())
    })?;
    hash_valid.for_each_set_bit(|index| {
        let directory = directories.get_mut(index)?;
        let (hash, rest) = data.split_at_checked(hash_len)?;
        data = rest;
        directory.exclude_file_oid = ObjectId::from_bytes_or_panic(hash).into();
        Some(())
    })?;

    // null-byte checked in the beginning
    if data.len() != 1 {
        return None;
    }
    res.into()
}

fn decode_directory_block<'a>(
    data: &'a [u8],
    directories: &mut Vec<Directory>,
    alloc_limit_bytes: Option<usize>,
) -> Option<&'a [u8]> {
    let (num_untracked, data) = var_int(data)?;
    let (num_dirs, data) = var_int(data)?;
    let (name, mut data) = split_at_byte_exclusive(data, 0)?;
    // Untracked names are encoded as `name\0name\0...`, and we assume names are non-empty:
    // `a\0b\0` is 4 bytes for 2 entries, so each entry needs at least 2 bytes.
    let max_entries_from_remaining_data = data.len() / 2;
    let num_untracked: usize = num_untracked.try_into().ok()?;
    let num_dirs: usize = num_dirs.try_into().ok()?;
    if num_untracked > max_entries_from_remaining_data || num_dirs > max_entries_from_remaining_data {
        return None;
    }
    if alloc_limit_bytes.is_some_and(|limit| {
        num_untracked.saturating_mul(std::mem::size_of::<BString>()) > limit
            || num_dirs.saturating_mul(std::mem::size_of::<usize>()) > limit
    }) {
        return None;
    }
    let mut untracked_entries = Vec::<BString>::new();
    untracked_entries.try_reserve(num_untracked).ok()?;
    for _ in 0..num_untracked {
        let (name, rest) = split_at_byte_exclusive(data, 0)?;
        data = rest;
        untracked_entries.push(name.into());
    }

    let index = directories.len();
    directories.push(Directory {
        name: name.into(),
        untracked_entries,
        sub_directories: {
            let mut sub_directories = Vec::new();
            sub_directories.try_reserve(num_dirs).ok()?;
            sub_directories
        },
        // the following are set later through their bitmaps
        stat: None,
        exclude_file_oid: None,
        check_only: false,
    });

    for _ in 0..num_dirs {
        let subdir_index = directories.len();
        let rest = decode_directory_block(data, directories, alloc_limit_bytes)?;
        data = rest;
        directories[index].sub_directories.push(subdir_index);
    }

    data.into()
}
