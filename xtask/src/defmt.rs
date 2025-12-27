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

fn format_frame(frame: &Frame, _locs: Option<&Locations>) -> String {
    let level = frame
        .level()
        .map(|l| l.as_str())
        .unwrap_or("print")
        .to_uppercase();

    format!("[{level:<5}] {}", frame.display_message())
}
