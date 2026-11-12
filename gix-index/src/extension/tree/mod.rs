use bstr::{BStr, ByteSlice};

use crate::extension::{Signature, Tree};

/// The signature for tree extensions
pub const SIGNATURE: Signature = *b"TREE";

///
pub mod verify;

mod decode;
pub use decode::decode;

mod write;

impl Tree {
    /// Invalidate the cached directory containing `path` and each of its ancestors.
    ///
    /// Cached sibling directories remain valid. If `path` names a cached directory itself,
    /// remove that directory because the index entry may now replace it with a file.
    pub fn invalidate_path(&mut self, path: &BStr) {
        self.num_entries = None;

        let Some((component, remainder)) = path.split_once_str("/") else {
            if let Ok(position) = self
                .children
                .binary_search_by(|child| child.name.as_slice().cmp(path.as_ref()))
            {
                self.children.remove(position);
            }
            return;
        };

        if let Ok(position) = self
            .children
            .binary_search_by(|child| child.name.as_slice().cmp(component))
        {
            self.children[position].invalidate_path(remainder.as_bstr());
        }
    }
}

#[cfg(test)]
mod tests {
    use gix_testtools::size_ok;

    #[test]
    fn size_of_tree() {
        let actual = std::mem::size_of::<crate::extension::Tree>();
        let sha1 = 88;
        let sha256_extra = 16;
        let expected = sha1 + sha256_extra;
        assert!(
            size_ok(actual, expected),
            "the size of this structure should not change unexpectedly: {actual} <~ {expected}"
        );
    }
}
