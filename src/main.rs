use clap::{Parser, Subcommand};
use nix::fcntl::copy_file_range;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Error, Read, Seek, Write};
use std::os::fd::AsRawFd;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a block-level diff between two files
    Create {
        /// Path where to write the bdiff output
        bdiff_output: String,
        /// Path to the target file to diff
        target_file: String,
        /// Path to the base file to compare against (optional)
        #[arg(long)]
        base: Option<String>,
    },
    /// Apply a block-level diff to create a new file
    Apply {
        /// Path to the bdiff file to apply
        bdiff_input: String,
        /// Path where to create the target file
        target_file: String,
        /// Path to the base file (optional)
        #[arg(long)]
        base: Option<String>,
    },
    /// View header information & extent map from a bdiff file
    View {
        /// Path to the bdiff file to inspect
        bdiff_input: String,
        /// Optional hex offset to filter ranges around (±1MB)
        offset: Option<String>,
    },
}

/// Represents a range in the target file that's different from the base file (as indicated by the CoW metadata)
#[derive(Debug, Serialize, Deserialize)]
struct DiffRange {
    logical_offset: u64,
    length: u64,
}

/// Magic string for bdiff files ("BDIFFv1\0")
const MAGIC: &[u8; 8] = b"BDIFFv1\0";

/// Standard block size used for alignment (4 KiB)
const BLOCK_SIZE: usize = 4096;

/// Calculate padding size needed to align to block boundary
fn calculate_padding_size(header_size: usize) -> usize {
    (BLOCK_SIZE - (header_size % BLOCK_SIZE)) % BLOCK_SIZE
}

/// Represents the header of a bdiff file. The file format is:
/// - Header:
///   - 8 bytes: magic string ("BDIFFv1\0")
///   - 8 bytes: target file size (little-endian)
///   - 8 bytes: base file size (little-endian)
///   - 8 bytes: number of ranges (little-endian)
///   - Ranges array, each range containing:
///     - 8 bytes: logical offset (little-endian)
///     - 8 bytes: length (little-endian)
/// - Padding to next block boundary (4 KiB)
/// - Range data (contiguous blocks of data)
#[derive(Debug, Serialize, Deserialize)]
struct BDiffHeader {
    magic: [u8; 8],
    target_size: u64,
    base_size: u64,
    ranges: Vec<DiffRange>,
}

impl BDiffHeader {
    fn new(target_size: u64, base_size: u64, ranges: Vec<DiffRange>) -> Self {
        Self {
            magic: *MAGIC,
            target_size,
            base_size,
            ranges,
        }
    }

    fn write_to(&self, writer: impl Write) -> Result<(), Error> {
        bincode::serialize_into(writer, self).map_err(|e| Error::new(std::io::ErrorKind::Other, e))
    }

    fn read_from(reader: impl Read) -> Result<Self, Error> {
        let header: Self = bincode::deserialize_from(reader)
            .map_err(|e| Error::new(std::io::ErrorKind::Other, e))?;

        if header.magic != *MAGIC {
            return Err(Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid bdiff file format",
            ));
        }

        Ok(header)
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

    if bytes == 0 {
        return "0 B".to_string();
    }

    // Find the appropriate unit (how many times can we divide by 1024)
    let exp = (bytes as f64).ln() / 1024_f64.ln();
    let exp = exp.floor() as usize;
    let exp = exp.min(UNITS.len() - 1); // Don't exceed available units

    // Convert to the chosen unit
    let bytes = bytes as f64 / (1024_u64.pow(exp as u32) as f64);

    // Format with 1 decimal place if >= 1024 bytes, otherwise no decimal
    if exp == 0 {
        format!("{} {}", bytes.round(), UNITS[exp])
    } else {
        format!("{:.1} {}", bytes, UNITS[exp])
    }
}

fn get_different_ranges(
    target_file: &str,
    base_file: Option<&str>,
) -> Result<Vec<DiffRange>, Error> {
    let mut diff_ranges = Vec::new();

    // Get fiemap for target file
    let mut target_extents = fiemap::fiemap(target_file)?.collect::<Result<Vec<_>, _>>()?;
    target_extents.sort_by_key(|e| e.fe_logical);

    // Check for any unsafe/unsupported flags
    for extent in &target_extents {
        use fiemap::FiemapExtentFlags as Flags;

        let unsafe_flags = [
            // Flags that indicate the file needs syncing
            (
                Flags::DELALLOC,
                "File has pending delayed allocations. Please sync file and try again",
            ),
            (
                Flags::UNWRITTEN,
                "File has unwritten extents. Please sync file and try again",
            ),
            (
                Flags::NOT_ALIGNED,
                "File has unaligned extents. Please sync file and try again",
            ),
            // Flags that indicate unsupported features
            (
                Flags::UNKNOWN,
                "Data location is unknown which is not supported",
            ),
            (
                Flags::ENCODED,
                "File contains encoded data which is not supported",
            ),
            (
                Flags::DATA_ENCRYPTED,
                "File contains encrypted data which is not supported",
            ),
        ];

        for (flag, message) in unsafe_flags {
            if extent.fe_flags.contains(flag) {
                return Err(Error::new(
                    std::io::ErrorKind::Other,
                    format!(
                        "Unsafe file state: extent at offset {:#x} has {:?} flag. {}.",
                        extent.fe_logical, flag, message
                    ),
                ));
            }
        }
    }

    // If no base file, return all non-empty extents
    if base_file.is_none() {
        for extent in target_extents {
            diff_ranges.push(DiffRange {
                logical_offset: extent.fe_logical,
                length: extent.fe_length,
            });
        }
        return Ok(diff_ranges);
    }

    // Get fiemap for base file
    let mut base_extents = fiemap::fiemap(base_file.unwrap())?.collect::<Result<Vec<_>, _>>()?;
    base_extents.sort_by_key(|e| e.fe_logical);

    // Total size of target file
    let total_size: u64 = target_extents.iter().map(|e| e.fe_length).sum();
    println!("Size of target file: {}", format_size(total_size));

    // Total size of base file
    let total_size: u64 = base_extents.iter().map(|e| e.fe_length).sum();
    println!("Size of base file: {}", format_size(total_size));

    // A helper closure for getting the end of any extent quickly
    let extent_end = |e: &fiemap::FiemapExtent| e.fe_logical + e.fe_length;

    // Index for base_extents
    let mut i = 0;

    'target_loop: for target_extent in target_extents {
        let mut current_start = target_extent.fe_logical;
        let mut current_remaining = target_extent.fe_length;

        // If this is a non-shared extent, it's entirely different.
        if !target_extent
            .fe_flags
            .contains(fiemap::FiemapExtentFlags::SHARED)
        {
            diff_ranges.push(DiffRange {
                logical_offset: current_start,
                length: current_remaining,
            });
            continue;
        }

        // Shared extent: we need to check partial overlaps with base_extents
        while current_remaining > 0 {
            // Skip any base extents that end before our current offset
            while i < base_extents.len() && extent_end(&base_extents[i]) <= current_start {
                i += 1;
            }
            // If we've consumed all base extents, everything left is different
            if i >= base_extents.len() {
                diff_ranges.push(DiffRange {
                    logical_offset: current_start,
                    length: current_remaining,
                });
                continue 'target_loop; // Move on to the next target extent
            }

            // Now, base_extents[i] is the first base extent that could overlap our target_extent
            let base_extent = &base_extents[i];
            let base_start = base_extent.fe_logical;
            let base_end = extent_end(base_extent);

            // If base_start > current_start, there's a gap in base coverage. Mark the gap as different.
            if base_start > current_start {
                let gap_len = (base_start - current_start).min(current_remaining);
                diff_ranges.push(DiffRange {
                    logical_offset: current_start,
                    length: gap_len,
                });
                current_start += gap_len;
                current_remaining -= gap_len;
                if current_remaining == 0 {
                    // done with this target extent
                    continue 'target_loop;
                }
            }

            // Compute overlap boundaries
            let overlap_start = current_start.max(base_start);
            let overlap_end = (current_start + current_remaining).min(base_end);

            // If there's no overlap, then the remainder of target_extent is all different
            if overlap_start >= overlap_end {
                diff_ranges.push(DiffRange {
                    logical_offset: current_start,
                    length: current_remaining,
                });
                continue 'target_loop;
            }

            // Physical offset for each file at overlap_start
            let current_physical_start =
                target_extent.fe_physical + (overlap_start - target_extent.fe_logical);
            let base_physical_start =
                base_extent.fe_physical + (overlap_start - base_extent.fe_logical);
            let overlap_len = overlap_end - overlap_start;

            // If physical offsets match, we consider that region "the same" and skip it
            if current_physical_start == base_physical_start {
                // "Consume" this overlap (not added to diff)
                current_start = overlap_end;
                current_remaining -= overlap_len;
            } else {
                // This overlap is different
                diff_ranges.push(DiffRange {
                    logical_offset: overlap_start,
                    length: overlap_len,
                });
                // Move past the overlap in the target
                current_start = overlap_end;
                current_remaining -= overlap_len;
            }

            // If we've consumed the entire base extent in that overlap, move on
            if overlap_end == base_end {
                i += 1;
            }
        }
    }

    Ok(diff_ranges)
}

/// Copies all bytes from src_fd to dst_fd, handling partial copies and interrupts.
/// Returns the total number of bytes copied.
fn copy_range(
    src_fd: std::os::unix::io::RawFd,
    mut src_offset: Option<&mut i64>,
    dst_fd: std::os::unix::io::RawFd,
    mut dst_offset: Option<&mut i64>,
    length: usize,
) -> Result<usize, Error> {
    let mut copied_total = 0;

    while copied_total < length {
        let copied = copy_file_range(
            src_fd,
            src_offset.as_deref_mut(),
            dst_fd,
            dst_offset.as_deref_mut(),
            length - copied_total,
        )
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;

        if copied == 0 {
            return Err(Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "Unexpected EOF: copied {} bytes, expected {}",
                    copied_total, length
                ),
            ));
        }

        copied_total += copied;
    }

    Ok(copied_total)
}

fn create_diff(
    bdiff_output: &str,
    target_file: &str,
    base_file: Option<&str>,
) -> Result<(), Error> {
    // 1) Open the target file so we can copy bytes from it later
    let target = File::open(target_file).map_err(|e| {
        Error::new(
            e.kind(),
            format!("Failed to open target file '{}': {}", target_file, e),
        )
    })?;

    // Sync the target file to ensure all delayed allocations are resolved
    nix::unistd::fsync(target.as_raw_fd()).map_err(|e| Error::new(std::io::ErrorKind::Other, e))?;

    let base = if let Some(base) = base_file {
        File::open(base).map_err(|e| {
            Error::new(
                e.kind(),
                format!("Failed to open base file '{}': {}", base, e),
            )
        })?
    } else {
        File::open(target_file).map_err(|e| {
            Error::new(
                e.kind(),
                format!("Failed to open target file '{}': {}", target_file, e),
            )
        })?
    };
    let target_size = target.metadata()?.len();
    let base_size = base.metadata()?.len();

    // 2) Compute the diff ranges
    let diff_ranges = get_different_ranges(target_file, base_file)?;
    let total_size: u64 = diff_ranges.iter().map(|range| range.length).sum();
    println!("Size of blockdiff: {}", format_size(total_size));

    // 3) Create the bdiff file
    let mut diff_out = File::create(bdiff_output).map_err(|e| {
        Error::new(
            e.kind(),
            format!("Failed to create bdiff file '{}': {}", bdiff_output, e),
        )
    })?;

    // 4) Create and write the header
    let header = BDiffHeader::new(target_size, base_size, diff_ranges);
    header.write_to(&mut diff_out)?;

    // 5) Pad with zeros to align header to block boundary
    let header_size = bincode::serialized_size(&header)
        .map_err(|e| Error::new(std::io::ErrorKind::Other, e))? as usize;
    let padding_size = calculate_padding_size(header_size);
    let padding = vec![0u8; padding_size];
    diff_out.write_all(&padding)?;

    // 6) Write all data blocks contiguously after the header
    for range in &header.ranges {
        let mut off_in = range.logical_offset as i64;

        let copied = copy_range(
            target.as_raw_fd(),
            Some(&mut off_in),
            diff_out.as_raw_fd(),
            None,
            range.length as usize,
        )?;

        if copied != range.length as usize {
            return Err(Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("Failed to copy all requested bytes for range {:?}: copied {} bytes, expected {}", 
                    range, copied, range.length)
            ));
        }
    }

    println!("Successfully created blockdiff file at {}", bdiff_output);

    Ok(())
}

fn apply_diff(bdiff_input: &str, target_file: &str, base_file: Option<&str>) -> Result<(), Error> {
    // Open the diff file and read header
    let mut diff_in = File::open(bdiff_input).map_err(|e| {
        Error::new(
            e.kind(),
            format!("Failed to open bdiff file '{}': {}", bdiff_input, e),
        )
    })?;
    let header = BDiffHeader::read_from(&mut diff_in)?;

    // Create target file (either as reflink copy of base or empty sparse file)
    let target = File::options()
        .write(true)
        .create(true)
        .open(target_file)
        .map_err(|e| {
            Error::new(
                e.kind(),
                format!("Failed to create target file '{}': {}", target_file, e),
            )
        })?;

    if let Some(base) = base_file {
        // Create as reflink copy of base
        let src = File::open(base).map_err(|e| {
            Error::new(
                e.kind(),
                format!("Failed to open base file '{}': {}", base, e),
            )
        })?;
        let total_len = src.metadata()?.len() as usize;
        let copied = copy_range(src.as_raw_fd(), None, target.as_raw_fd(), None, total_len)?;

        if copied != total_len {
            return Err(Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("Failed to create target file {} as copy of base file {}: copied {} bytes, expected {}", 
                    target_file, base, copied, total_len)
            ));
        }

        println!(
            "Initialized target file as reflink copy of base file at: {}",
            target_file
        );

        // Check if target size differs from base size and resize if needed
        if header.target_size != header.base_size {
            println!(
                "Note: target file size differs from base file size: {} -> {}",
                format_size(header.base_size),
                format_size(header.target_size)
            );
            target.set_len(header.target_size)?;
        }
    } else {
        // Create empty sparse file of target size
        target.set_len(header.target_size)?;
        println!(
            "Initialized target file as empty sparse file of size {} at: {}",
            format_size(header.target_size),
            target_file
        );
    }

    // Skip padding to align with block boundary
    let header_size = bincode::serialized_size(&header)
        .map_err(|e| Error::new(std::io::ErrorKind::Other, e))? as usize;
    let padding_size = calculate_padding_size(header_size);
    diff_in.seek(std::io::SeekFrom::Current(padding_size as i64))?;

    // Apply each range
    for range in header.ranges {
        let mut off_out = range.logical_offset as i64;
        let copied = copy_range(
            diff_in.as_raw_fd(),
            None,
            target.as_raw_fd(),
            Some(&mut off_out),
            range.length as usize,
        )?;

        if copied != range.length as usize {
            return Err(Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("Failed to copy all requested bytes for range {:?}: copied {} bytes, expected {}", 
                    range, copied, range.length)
            ));
        }
    }

    println!("Successfully applied {} to target file", bdiff_input);

    Ok(())
}

fn debug_viewer(input_file: &str, offset_str: Option<&str>) -> Result<(), Error> {
    // Parse the hex offset if provided
    let filter_offset = if let Some(off_str) = offset_str {
        // Remove "0x" prefix if present and parse
        let cleaned = off_str.trim_start_matches("0x");
        Some(u64::from_str_radix(cleaned, 16).map_err(|e| {
            Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid hex offset: {}", e),
            )
        })?)
    } else {
        None
    };

    if input_file.ends_with(".bdiff") {
        let diff_in = File::open(input_file).map_err(|e| {
            Error::new(
                e.kind(),
                format!("Failed to open bdiff file '{}': {}", input_file, e),
            )
        })?;
        let header = BDiffHeader::read_from(diff_in)?;

        println!("BDiff File: {}", input_file);
        println!("Magic: {:?}", String::from_utf8_lossy(&header.magic));
        println!("Target file size: {}", format_size(header.target_size));
        println!("Base file size: {}", format_size(header.base_size));
        println!("Number of ranges: {}", header.ranges.len());

        let total_diff_size: u64 = header.ranges.iter().map(|r| r.length).sum();
        println!("Total diff size: {}", format_size(total_diff_size));

        println!("\nRanges:");
        if let Some(offset) = filter_offset {
            // Find the range containing the offset
            let containing_idx = header
                .ranges
                .iter()
                .position(|r| r.logical_offset <= offset && offset < r.logical_offset + r.length);

            if let Some(idx) = containing_idx {
                // Show 3 ranges before and after
                let start_idx = idx.saturating_sub(3);
                let end_idx = (idx + 4).min(header.ranges.len());

                for i in start_idx..end_idx {
                    let range = &header.ranges[i];
                    println!(
                        "  {}{}: offset={:#x} length={:#x} ({})",
                        if i == idx { ">" } else { " " },
                        i,
                        range.logical_offset,
                        range.length,
                        format_size(range.length)
                    );
                }
            } else {
                println!("  No range contains offset {:#x}", offset);
            }
        } else {
            // Show all ranges when no filter
            for (i, range) in header.ranges.iter().enumerate() {
                println!(
                    "  {}: offset={:#x} length={:#x} ({})",
                    i,
                    range.logical_offset,
                    range.length,
                    format_size(range.length)
                );
            }
        }
    } else {
        println!("File: {}", input_file);

        let mut extents = fiemap::fiemap(input_file)?.collect::<Result<Vec<_>, _>>()?;
        extents.sort_by_key(|e| e.fe_logical);

        let total_size: u64 = extents.iter().map(|e| e.fe_length).sum();
        println!("Total file size: {}", format_size(total_size));
        println!("Number of extents: {}", extents.len());

        println!("\nExtents:");
        if let Some(offset) = filter_offset {
            // Find the extent containing the offset
            let containing_idx = extents
                .iter()
                .position(|e| e.fe_logical <= offset && offset < e.fe_logical + e.fe_length);

            if let Some(idx) = containing_idx {
                // Show 3 extents before and after
                let start_idx = idx.saturating_sub(3);
                let end_idx = (idx + 4).min(extents.len());

                for i in start_idx..end_idx {
                    let extent = &extents[i];
                    println!(
                        "  {}{}: logical={:#x} physical={:#x} length={:#x} ({}) flags={:?}",
                        if i == idx { ">" } else { " " },
                        i,
                        extent.fe_logical,
                        extent.fe_physical,
                        extent.fe_length,
                        format_size(extent.fe_length),
                        extent.fe_flags
                    );
                }
            } else {
                println!("  No extent contains offset {:#x}", offset);
            }
        } else {
            // Show all extents when no filter
            for (i, extent) in extents.iter().enumerate() {
                println!(
                    "  {}: logical={:#x} physical={:#x} length={:#x} ({}) flags={:?}",
                    i,
                    extent.fe_logical,
                    extent.fe_physical,
                    extent.fe_length,
                    format_size(extent.fe_length),
                    extent.fe_flags
                );
            }
        }
    }

    Ok(())
}

fn main() -> Result<(), Error> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Create {
            bdiff_output,
            target_file,
            base,
        } => create_diff(bdiff_output, target_file, base.as_deref()),
        Commands::Apply {
            bdiff_input,
            target_file,
            base,
        } => apply_diff(bdiff_input, target_file, base.as_deref()),
        Commands::View {
            bdiff_input,
            offset,
        } => debug_viewer(bdiff_input, offset.as_deref()),
    }
}
