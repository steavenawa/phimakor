//! Numeric check: where does a note's quad land vs the fx burst, per aspect.
//! Prints world positions of note & fx computed exactly like the renderer.

use phimakor::core::chart::Chart;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "example_chart".into());
    let (_info, mut chart) = Chart::load(std::path::Path::new(&dir)).unwrap();
    let t = chart.duration() * 0.3;
    let frame = chart.state_at(t);

    let window_aspect = 16.0 / 9.0;
    for (name, aspect) in [("3:2", 1.5f32), ("16:9", 16.0 / 9.0), ("4:3", 4.0 / 3.0), ("1:1", 1.0)] {
        let (kx, ky) = if window_aspect >= aspect { (aspect / window_aspect, 1.0) } else { (1.0, window_aspect / aspect) };
        let fit = (1.5 / aspect).min(1.0);
        let ev_x = aspect / 1.5;
        // letterbox = (kx*fit/675, ky*1.5/675); x positions ×ev_x
        let lx = kx * fit / 675.0;
        let ly = ky * 1.5 / 675.0;
        // box top edge: canvas y = 450 should hit world ky at every aspect
        let y_top = 450.0 * ly;

        // first visible note
        let (_li, line, note) = frame.lines.iter().enumerate()
            .find_map(|(li, l)| l.notes.first().map(|n| (li, l, n))).unwrap();
        // note world pos (renderer): letterbox * T(line_px) * R * T(note_px)
        let ctrl_px = line.position[0] * 675.0 * ev_x;
        let ctrl_py = line.position[1] * 450.0;
        let nx = note.relative[0] * 675.0 * ev_x;
        let ny = note.relative[1] * 450.0;
        let (cos, sin) = (line.rotation.cos(), line.rotation.sin());
        let nwx = lx * (ctrl_px + cos * nx - sin * ny);
        let nwy = ly * (ctrl_py + sin * nx + cos * ny);
        // fx world pos: letterbox * T(cx*ev_x, cy)
        let cx = (line.position[0] + cos * note.relative[0]) * 675.0;
        let cy = (line.position[1] + sin * note.relative[0]) * 450.0;
        let fwx = lx * cx * ev_x;
        let fwy = ly * cy;
        println!(
            "{name:5} fit={fit:.3} ev_x={ev_x:.3} | y_top(450)={y_top:.3} (box edge ky={ky:.3}) | note ({nwx:+.3},{nwy:+.3}) fx ({fwx:+.3},{fwy:+.3}) | Δ({:.3},{:.3})",
            fwx - nwx, fwy - nwy
        );
    }
}
