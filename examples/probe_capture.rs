//! Probe: is DXGI desktop duplication giving us SHARP pixels?
//! Saves the screen center to target/probe_capture.png — inspect by eye:
//! text must be crisp, colors untinted. This is the gate for the whole
//! engine; if this image is blurry the architecture is wrong.

use liquidnotes::gpu::{capture::Capture, device::Gpu, read_region};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gpu = Gpu::new()?;
    let mut cap = Capture::new(&gpu)?;
    println!(
        "duplication live: {}x{} at virtual origin {:?}",
        cap.width, cap.height, cap.origin
    );

    // Exercise the real reconstruction path (tick), not the diagnostic copy.
    for _ in 0..300 {
        cap.tick(&[]);
        if cap.seeded() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    if !cap.seeded() {
        println!("note: no image frame via tick (static screen); forcing one");
        cap.force_full_refresh(2000)?;
    }

    let w = 900.min(cap.width);
    let h = 560.min(cap.height);
    let x = (cap.width - w) / 2;
    let y = (cap.height - h) / 2;
    let bgra = read_region(&gpu.device, &gpu.context, &cap.background, x, y, w, h)?;

    let mut rgba = bgra;
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    // Sanity: reject an all-black readback.
    let lit = rgba.chunks_exact(4).filter(|p| p[0] as u32 + p[1] as u32 + p[2] as u32 > 30).count();
    println!("non-dark pixels: {} / {}", lit, (w * h) as usize);

    std::fs::create_dir_all("target")?;
    let file = std::fs::File::create("target/probe_capture.png")?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&rgba)?;
    println!("saved target/probe_capture.png ({w}x{h})");
    Ok(())
}
