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
        let _fit = (1.5 / aspect).min(1.0);
        // Event positions depend ONLY on the playfield aspect (never the
        // window): the canvas edges (675/450) land on the box edges (±kx, ±ky)
        // at any window. Sprites keep the uniform letterbox scale (sx/sy).
        let ev_x = 1.0;
        let ev_y = 1.5 / aspect;
        // Uniform letterbox: (kx/675, ky*aspect/675); positions ×(ev_x, ev_y)
        let lx = kx / 675.0;
        let ly = ky * aspect / 675.0;
        // box edges in world units: canvas edges (675/450) under the fill map
        // must land exactly on ±kx/±ky; sprite px/px must stay uniform.
        let x_edge = 675.0 * ev_x * lx;
        let y_edge = 450.0 * ev_y * ly;
        // sprite uniformity: screen px per canvas px on both axes
        let sx = lx * 640.0;
        let sy = ly * 360.0;

        // first visible note
        let (_li, line, note) = frame.lines.iter().enumerate()
            .find_map(|(li, l)| l.notes.first().map(|n| (li, l, n))).unwrap();
        // note world pos (renderer): letterbox * T(line_px) * R * T(note_px)
        let ctrl_px = line.position[0] * 675.0 * ev_x;
        let ctrl_py = line.position[1] * 450.0 * ev_y;
        let nx = note.relative[0] * 675.0 * ev_x;
        let ny = note.relative[1] * 450.0 * ev_y;
        let (cos, sin) = (line.rotation.cos(), line.rotation.sin());
        let nwx = lx * (ctrl_px + cos * nx - sin * ny);
        let nwy = ly * (ctrl_py + sin * nx + cos * ny);
        // fx world pos: letterbox * T(cx*ev_x, cy*ev_y); cy uses the canvas-X
        // rotation term ×playfield aspect so the burst lands on the note center.
        let cx = (line.position[0] + cos * note.relative[0]) * 675.0;
        let cy = (line.position[1] + sin * note.relative[0] * aspect) * 450.0;
        let fwx = lx * cx * ev_x;
        let fwy = ly * cy * ev_y;
        println!(
            "{name:5} ev_x={ev_x:.3} ev_y={ev_y:.3} | edges=({x_edge:.3},{y_edge:.3}) (kx={kx:.3},ky={ky:.3}) | sprite px/px ({sx:.3},{sy:.3}) | Δ({:.3},{:.3})",
            fwx - nwx, fwy - nwy
        );
    }
}
