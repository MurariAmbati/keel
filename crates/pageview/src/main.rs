//! `pageview <file> <page>` — a schema-aware hexdump of one page (D12).
//!
//! The debugging instrument you live inside during the storage phases: it
//! interprets the header, lists the slot directory, annotates heap records by
//! kind, and flags a checksum mismatch loudly.

use std::process::ExitCode;

use keel_heap::{classify_record, RecordKind};
use keel_page::{SlottedPage, PAGE_SIZE};
use keel_vfs::{BlockFile, OsFile};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let path = match args.next() {
        Some(p) => p,
        None => {
            eprintln!("usage: pageview <data-file> <page-number> [--hex]");
            return ExitCode::from(2);
        }
    };
    let page: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let show_hex = args.any(|a| a == "--hex");

    let file = match OsFile::open_readonly(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("pageview: cannot open {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let mut buf = vec![0u8; PAGE_SIZE];
    if let Err(e) = file.read_at(&mut buf, page as u64 * PAGE_SIZE as u64) {
        eprintln!("pageview: cannot read page {page}: {e}");
        return ExitCode::from(2);
    }

    let sp = SlottedPage::from_bytes(&buf[..]);
    let ck_ok = sp.verify_checksum();
    println!("== page {page} ==");
    println!(
        "type={:?} version={} lsn={} flags=0x{:04x} extra={}",
        sp.page_type(),
        sp.format_version(),
        sp.page_lsn(),
        sp.flags(),
        sp.extra(),
    );
    println!(
        "slots={} live={} free_start={} free_end={} free_space={} compactable_free={}",
        sp.slot_count(),
        sp.live_count(),
        sp.free_start(),
        sp.free_end(),
        sp.free_space(),
        sp.compactable_free(),
    );
    println!(
        "checksum stored=0x{:08x} computed=0x{:08x} {}",
        sp.stored_checksum(),
        sp.computed_checksum(),
        if ck_ok { "OK" } else { "*** MISMATCH ***" }
    );
    match sp.validate_structure() {
        Ok(()) => println!("structure: OK"),
        Err(e) => println!("structure: *** {e} ***"),
    }

    println!("-- slot directory --");
    for (slot, bytes) in sp.iter() {
        let annotation = match classify_record(bytes) {
            Ok(RecordKind::Tuple) => "tuple".to_string(),
            Ok(RecordKind::Forward(target)) => format!("forward -> {target:?}"),
            Ok(RecordKind::ForwardTarget) => "forward-target".to_string(),
            Err(tag) => format!("?? tag={tag}"),
        };
        let preview: String = bytes
            .iter()
            .take(24)
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!(
            "  [{slot:>4}] len={:>4} {annotation:<24} {preview}",
            bytes.len()
        );
    }

    if show_hex {
        println!("-- hex --");
        hexdump(&buf);
    }

    if ck_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn hexdump(buf: &[u8]) {
    for (i, chunk) in buf.chunks(16).enumerate() {
        let off = i * 16;
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("{off:08x}  {:<47}  {ascii}", hex.join(" "));
    }
}
