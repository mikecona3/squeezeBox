use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::process::exit;
use std::time::Instant;

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression as ZlibCompression;

use bzip2::read::BzDecoder;
use bzip2::write::BzEncoder;
use bzip2::Compression as BzCompression;

use xz2::read::XzDecoder;
use xz2::stream::MtStreamBuilder;
use xz2::write::XzEncoder;

use memmap2::Mmap;


const ZLIB_SKIP_THRESHOLD: usize = 4 * 1024 * 1024;
const METHOD_XZ: u8 = b'L';
const METHOD_BZ2: u8 = b'B';
const METHOD_ZLIB: u8 = b'Z';
const METHOD_RAW: u8 = b'R';

fn human_size(n: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut size = n;
    for unit in units.iter() {
        if size < 1024.0 {
            return format!("{:.1}{}", size, unit);
        }
        size /= 1024.0;
    }
    format!("{:.1}TB", size)
}

fn compress_xz(data: &[u8], threads: u32) -> std::io::Result<Vec<u8>> {
    // preset extreme flag
    let preset = 9 | 0x80000000;

    // files >1mib are skipped, return to single threaded encoder
    if threads <= 1 || data.len() < 1024 * 1024 {
        let mut encoder = XzEncoder::new(Vec::new(), preset);
        encoder.write_all(data)?;
        return encoder.finish();
    }

    let stream = MtStreamBuilder::new()
        .threads(threads)
        .preset(preset)
        .encoder()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let mut encoder = XzEncoder::new_stream(Vec::new(), stream);
    encoder.write_all(data)?;
    encoder.finish()
}

fn compress_bz2(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = BzEncoder::new(Vec::new(), BzCompression::best());
    encoder.write_all(data)?;
    encoder.finish()
}

fn compress_zlib(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), ZlibCompression::best());
    encoder.write_all(data)?;
    encoder.finish()
}

fn decompress_xz(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoder = XzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

fn decompress_bz2(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoder = BzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

fn decompress_zlib(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

fn compress_file(input_path: &str, output_path: &str) -> std::io::Result<()> {
    let file = File::open(input_path)?;
    let original_size = file.metadata()?.len() as usize;

    if original_size == 0 {
        fs::write(output_path, [METHOD_RAW])?;
        println!("Input file is empty. Nothing to compress.");
        return Ok(());
    }

    // saves memory on large files by avoiding a full read into a Vec
    let mmap = unsafe { Mmap::map(&file)? };
    let data: std::sync::Arc<Mmap> = std::sync::Arc::new(mmap);

    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);

    let run_zlib = original_size <= ZLIB_SKIP_THRESHOLD;

    // runs all algoirthms concurrently on sep threads
    let t0_all = Instant::now();

    let data_xz = std::sync::Arc::clone(&data);
    let xz_handle = std::thread::spawn(move || {
        let t0 = Instant::now();
        let result = compress_xz(&data_xz, cpus);
        (result, t0.elapsed())
    });

    let data_bz2 = std::sync::Arc::clone(&data);
    let bz2_handle = std::thread::spawn(move || {
        let t0 = Instant::now();
        let result = compress_bz2(&data_bz2);
        (result, t0.elapsed())
    });

    let zlib_handle = if run_zlib {
        let data_zlib = std::sync::Arc::clone(&data);
        Some(std::thread::spawn(move || {
            let t0 = Instant::now();
            let result = compress_zlib(&data_zlib);
            (result, t0.elapsed())
        }))
    } else {
        None
    };

    let (xz_result, t_xz) = xz_handle.join().expect("xz thread panicked");
    let (bz2_result, t_bz2) = bz2_handle.join().expect("bz2 thread panicked");
    let xz_result = xz_result?;
    let bz2_result = bz2_result?;

    let (zlib_result, t_zlib): (Option<Vec<u8>>, std::time::Duration) = match zlib_handle {
        Some(h) => {
            let (result, t) = h.join().expect("zlib thread panicked");
            (Some(result?), t)
        }
        None => (None, std::time::Duration::ZERO),
    };

    let t_wall = t0_all.elapsed();

    let mut best_method = METHOD_XZ;
    let mut best_data: &[u8] = &xz_result;

    if bz2_result.len() < best_data.len() {
        best_method = METHOD_BZ2;
        best_data = &bz2_result;
    }
    if let Some(ref zlib_data) = zlib_result {
        if zlib_data.len() < best_data.len() {
            best_method = METHOD_ZLIB;
            best_data = zlib_data;
        }
    }

    let (final_method, final_data): (u8, &[u8]) = if best_data.len() >= original_size {
        (METHOD_RAW, &data[..])
    } else {
        (best_method, best_data)
    };

    let mut out = Vec::with_capacity(final_data.len() + 1);
    out.push(final_method);
    out.extend_from_slice(final_data);
    fs::write(output_path, &out)?;

    let final_size = final_data.len() + 1;
    let ratio = (1.0 - final_size as f64 / original_size as f64) * 100.0;

    let method_name = match final_method {
        METHOD_XZ => "XZ (LZMA2, multi-threaded)",
        METHOD_BZ2 => "BZIP2",
        METHOD_ZLIB => "ZLIB",
        _ => "stored raw",
    };

    println!("Original size:   {}", human_size(original_size as f64));
    print!(
        "Tried (in parallel): XZ={} ({:.2?}), BZIP2={} ({:.2?})",
        human_size(xz_result.len() as f64),
        t_xz,
        human_size(bz2_result.len() as f64),
        t_bz2,
    );
    match &zlib_result {
        Some(z) => println!(", ZLIB={} ({:.2?})", human_size(z.len() as f64), t_zlib),
        None => println!(
            ", ZLIB=skipped (file > {})",
            human_size(ZLIB_SKIP_THRESHOLD as f64)
        ),
    }
    println!(
        "Wall time:        {:.2?} (vs {:.2?} if run sequentially)",
        t_wall,
        t_xz + t_bz2 + t_zlib
    );
    println!("Best method:      {}", method_name);
    println!("Final size:       {}", human_size(final_size as f64));
    println!("Reduction:        {:.1}%", ratio);

    Ok(())
}

fn decompress_file(input_path: &str, output_path: &str) -> std::io::Result<()> {
    let raw = fs::read(input_path)?;
    if raw.is_empty() {
        eprintln!("Error: input file is empty or corrupted (missing method header).");
        exit(1);
    }
    let method = raw[0];
    let payload = &raw[1..];

    let data = match method {
        METHOD_XZ => decompress_xz(payload)?,
        METHOD_BZ2 => decompress_bz2(payload)?,
        METHOD_ZLIB => decompress_zlib(payload)?,
        METHOD_RAW => payload.to_vec(),
        _ => {
            eprintln!("Error: unknown or corrupted file (bad method header byte).");
            exit(1);
        }
    };

    fs::write(output_path, &data)?;
    println!("Restored {} -> {}", human_size(data.len() as f64), output_path);
    Ok(())
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  max_compress compress   <input_file> <output_file.mcz>");
    eprintln!("  max_compress decompress <output_file.mcz> <restored_file>");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        print_usage();
        exit(1);
    }

    let mode = args[1].as_str();
    let input_path = &args[2];
    let output_path = &args[3];

    if !std::path::Path::new(input_path).is_file() {
        eprintln!("Error: '{}' not found.", input_path);
        exit(1);
    }

    let result = match mode {
        "compress" => compress_file(input_path, output_path),
        "decompress" => decompress_file(input_path, output_path),
        _ => {
            print_usage();
            exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        exit(1);
    }
}
