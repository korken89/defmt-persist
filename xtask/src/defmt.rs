use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use defmt_decoder::{DecodeError, Frame, Locations, Table};

pub fn decode_output(elf_path: &Path, raw_output: &[u8]) -> Result<String> {
    let elf_data = fs::read(elf_path).context("Failed to read ELF file")?;
    let table = Table::parse(&elf_data)
        .context("Failed to parse defmt table from ELF")?
        .ok_or_else(|| anyhow::anyhow!("No defmt data found in ELF"))?;

    let locs = table.get_locations(&elf_data).ok();
    let locs = locs.as_ref();

    let mut decoder = table.new_stream_decoder();
    decoder.received(raw_output);

    let mut output = String::new();

    loop {
        match decoder.decode() {
            Ok(frame) => {
                let msg = format_frame(&frame, locs);
                output.push_str(&msg);
                output.push('\n');
            }
            Err(DecodeError::UnexpectedEof) => break,
            Err(DecodeError::Malformed) => {
                bail!("Malformed defmt frame");
            }
        }
    }

    Ok(output)
}

fn format_frame(frame: &Frame, locs: Option<&Locations>) -> String {
    let level = frame
        .level()
        .map(|l| l.as_str())
        .unwrap_or("print")
        .to_uppercase();

    let loc = locs.and_then(|locs| locs.get(&frame.index())).map(|loc| {
        // Extract just the filename, not the full path
        let filename = loc
            .file
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| loc.file.display().to_string());
        format!("{filename}:{}", loc.line)
    });

    match loc {
        Some(loc) => format!("{loc}: [{level:<5}] {}", frame.display_message()),
        None => format!("[{level:<5}] {}", frame.display_message()),
    }
}
