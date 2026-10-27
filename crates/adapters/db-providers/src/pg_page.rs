//! Comparing a PostgreSQL relation file against its committed copy, page by page,
//! disregarding the bytes PostgreSQL rewrites on a plain read.
//!
//! The first read of a row after its inserting transaction commits sets a hint
//! bit in the row's header and marks the page dirty, without WAL when data
//! checksums are off, and the page reaches disk at the next checkpoint or
//! shutdown. Measured on a restored workspace: `SELECT count(*)` over 1000 rows
//! changed 1000 bytes of the table's file, every one of them `t_infomask`'s
//! `HEAP_XMIN_COMMITTED`, and the next checkout refused.
//!
//! The bytes disregarded are those PostgreSQL itself treats as hints when it
//! checks a replayed page against the original (`mask_page_lsn_and_checksum`,
//! `mask_page_hint_bits` and `heap_mask` in its `bufmask.c` and `heapam.c`),
//! and only on heap pages. Anything else that differs is reported.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const PAGE_SIZE: usize = 8192;
const PAGE_HEADER_SIZE: usize = 24;
/// `pd_pagesize_version` of an 8 KiB page in the current layout (version 4).
const PAGE_SIZE_VERSION: u16 = 8192 | 4;
/// `PD_HAS_FREE_LINES | PD_PAGE_FULL | PD_ALL_VISIBLE`.
const PAGE_HINT_FLAGS: u16 = 0x0007;
/// `HEAP_XMIN_COMMITTED | HEAP_XMIN_INVALID | HEAP_XMAX_COMMITTED | HEAP_XMAX_INVALID`.
const TUPLE_HINT_BITS: u16 = 0x0F00;
const LP_NORMAL: u32 = 1;
/// Offset of `t_infomask` in a heap tuple header.
const INFOMASK_OFFSET: usize = 20;

/// Whether `relative_path` is a relation's main fork (`base/<db>/<node>[.<n>]` or
/// `global/<node>[.<n>]`), the only files laid out as heap or index pages.
pub(crate) fn is_main_fork(relative_path: &str) -> bool {
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let file = match relative_path.split('/').collect::<Vec<_>>()[..] {
        ["base", db, file] if all_digits(db) => file,
        ["global", file] => file,
        _ => return false,
    };
    match file.split_once('.') {
        Some((node, segment)) => all_digits(node) && all_digits(segment),
        None => all_digits(file),
    }
}

/// Whether `current` holds the same data as `committed`: the same length, and
/// every page identical or, for heap pages, identical once hints are masked.
pub(crate) fn same_data(committed: &Path, current: &Path) -> io::Result<bool> {
    let (mut committed, mut current) = (File::open(committed)?, File::open(current)?);
    let length = committed.metadata()?.len();
    if length != current.metadata()?.len() || length % PAGE_SIZE as u64 != 0 {
        return Ok(false);
    }
    let (mut before, mut after) = ([0u8; PAGE_SIZE], [0u8; PAGE_SIZE]);
    for _ in 0..length / PAGE_SIZE as u64 {
        committed.read_exact(&mut before)?;
        current.read_exact(&mut after)?;
        if before != after {
            let both_heap = mask_heap_page(&mut before) && mask_heap_page(&mut after);
            if !both_heap || before != after {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn u16_at(page: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([page[at], page[at + 1]])
}

/// Clear the hint bytes of a heap page in place; `false`, untouched, when the
/// page is not a heap page.
fn mask_heap_page(page: &mut [u8; PAGE_SIZE]) -> bool {
    let lower = u16_at(page, 12) as usize;
    let upper = u16_at(page, 14) as usize;
    let special = u16_at(page, 16) as usize;
    // A heap page has no special space; an index page always has some.
    if u16_at(page, 18) != PAGE_SIZE_VERSION
        || special != PAGE_SIZE
        || lower < PAGE_HEADER_SIZE
        || lower > upper
        || upper > PAGE_SIZE
    {
        return false;
    }
    page[0..10].fill(0); // pd_lsn, pd_checksum
    let flags = u16_at(page, 10) & !PAGE_HINT_FLAGS;
    page[10..12].copy_from_slice(&flags.to_le_bytes());
    page[20..24].fill(0); // pd_prune_xid
    for item in (PAGE_HEADER_SIZE..lower).step_by(4) {
        let Some(bytes) = page.get(item..item + 4) else {
            break;
        };
        let line_pointer = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let offset = (line_pointer & 0x7FFF) as usize;
        let infomask_at = offset + INFOMASK_OFFSET;
        if (line_pointer >> 15) & 0x3 != LP_NORMAL || infomask_at + 2 > PAGE_SIZE {
            continue;
        }
        let infomask = u16_at(page, infomask_at) & !TUPLE_HINT_BITS;
        page[infomask_at..infomask_at + 2].copy_from_slice(&infomask.to_le_bytes());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const XMIN_COMMITTED: u16 = 0x0100;
    const TUPLE_AT: usize = 8000;

    /// A heap page holding one normal tuple at `TUPLE_AT`.
    fn heap_page(infomask: u16, xmax: u32, value: u8) -> [u8; PAGE_SIZE] {
        let mut page = [0u8; PAGE_SIZE];
        page[12..14].copy_from_slice(&28u16.to_le_bytes()); // pd_lower: one line pointer
        page[14..16].copy_from_slice(&(TUPLE_AT as u16).to_le_bytes()); // pd_upper
        page[16..18].copy_from_slice(&(PAGE_SIZE as u16).to_le_bytes()); // pd_special
        page[18..20].copy_from_slice(&PAGE_SIZE_VERSION.to_le_bytes());
        let line_pointer = TUPLE_AT as u32 | (LP_NORMAL << 15) | (28 << 17);
        page[24..28].copy_from_slice(&line_pointer.to_le_bytes());
        page[TUPLE_AT..TUPLE_AT + 4].copy_from_slice(&739u32.to_le_bytes()); // t_xmin
        page[TUPLE_AT + 4..TUPLE_AT + 8].copy_from_slice(&xmax.to_le_bytes());
        page[TUPLE_AT + 20..TUPLE_AT + 22].copy_from_slice(&infomask.to_le_bytes());
        page[TUPLE_AT + 22] = 24; // t_hoff
        page[TUPLE_AT + 24] = value;
        page
    }

    fn compare(committed: &[u8], current: &[u8]) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, committed).unwrap();
        std::fs::write(&b, current).unwrap();
        same_data(&a, &b).unwrap()
    }

    #[test]
    fn a_hint_bit_set_by_a_read_is_the_same_data() {
        let committed = heap_page(0x0800, 0, 7);
        let mut read = heap_page(0x0800 | XMIN_COMMITTED, 0, 7);
        read[0..8].copy_from_slice(&42u64.to_le_bytes()); // LSN, as with checksums on
        assert!(compare(
            &[committed, committed].concat(),
            &[committed, read].concat()
        ));
    }

    #[test]
    fn a_changed_value_is_different_data() {
        let committed = heap_page(0x0800, 0, 7);
        assert!(!compare(&committed, &heap_page(0x0800, 0, 8)));
        assert!(!compare(
            &committed,
            &heap_page(0x0800 | XMIN_COMMITTED, 0, 8)
        ));
    }

    #[test]
    fn a_deleted_row_is_different_data() {
        let committed = heap_page(0x0800, 0, 7);
        assert!(!compare(&committed, &heap_page(XMIN_COMMITTED, 812, 7)));
    }

    #[test]
    fn an_index_page_is_compared_byte_for_byte() {
        let mut committed = heap_page(0x0800, 0, 7);
        let mut read = heap_page(0x0800 | XMIN_COMMITTED, 0, 7);
        for page in [&mut committed, &mut read] {
            page[16..18].copy_from_slice(&((PAGE_SIZE - 16) as u16).to_le_bytes());
        }
        assert!(!compare(&committed, &read));
    }

    #[test]
    fn files_of_different_length_are_different_data() {
        let page = heap_page(0x0800, 0, 7);
        assert!(!compare(&page, &[page, page].concat()));
    }

    #[test]
    fn only_a_relation_main_fork_is_paged() {
        for path in ["base/5/1249", "base/16384/16385.1", "global/1262"] {
            assert!(is_main_fork(path), "{path}");
        }
        for path in [
            "base/5/1249_vm",
            "base/5/1249_fsm",
            "base/5/pg_internal.init",
            "base/5/PG_VERSION",
            "global/pg_control",
            "pg_wal/000000010000000000000001",
            "base/x/1249",
            "base/5/1249.",
            "1249",
        ] {
            assert!(!is_main_fork(path), "{path}");
        }
    }
}
