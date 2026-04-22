// Carbon-footprint estimator for --carbon.
//
// Methodology:
//   Energy per token       ~0.001 Wh         arXiv:2505.09598, arXiv:2509.08867
//   Cache-read energy      ~1% of full       ccwatt methodology
//   Grid carbon intensity  0.390 kg CO₂/kWh  IEA 2024 global average
//   Tree CO₂ absorption    38.4 g/day        European Environment Agency (14 kg/yr)
//
// Cache-write is treated as a full-energy token (actual compute on the
// serving side); only cache-read gets the 1% discount.

pub const WH_PER_TOKEN: f64 = 0.001;
pub const CACHE_READ_RATIO: f64 = 0.01;
pub const GRID_KG_PER_KWH: f64 = 0.390;
pub const TREE_KG_PER_YEAR: f64 = 14.0;
pub const TREE_G_PER_DAY: f64 = 38.4;

pub struct Carbon {
    pub energy_kwh: f64,
    pub co2_kg: f64,
    pub tree_years: f64,
    pub tree_days: f64,
}

pub fn compute(tot_in: u64, tot_out: u64, tot_cache_w: u64, tot_cache_r: u64) -> Carbon {
    let full = (tot_in + tot_out + tot_cache_w) as f64;
    let cache_r = tot_cache_r as f64;
    let watt_hours = cache_r.mul_add(WH_PER_TOKEN * CACHE_READ_RATIO, full * WH_PER_TOKEN);
    let energy_kwh = watt_hours / 1000.0;
    let co2_kg = energy_kwh * GRID_KG_PER_KWH;
    let tree_years = co2_kg / TREE_KG_PER_YEAR;
    let tree_days = co2_kg * 1000.0 / TREE_G_PER_DAY;
    Carbon {
        energy_kwh,
        co2_kg,
        tree_years,
        tree_days,
    }
}

pub fn write_footer(buf: &mut String, c: &Carbon, box_width: usize) {
    // Wrap the carbon stats in the same box-drawing vocabulary as the
    // table above (┌ ├ └, ─, │) so the footer reads as part of the
    // report rather than a tacked-on paragraph. `box_width` is the
    // visible width of the table line — passed in by the caller so the
    // footer aligns under the totals row.
    use std::fmt::Write as _;
    // Inner content width: subtract two `│` and the two padding spaces.
    let inner = box_width.saturating_sub(4);
    let dashes: String = "─".repeat(box_width.saturating_sub(2));
    let _ = writeln!(buf);
    let _ = writeln!(buf, "┌{dashes}┐");
    let title = "Carbon footprint  (arXiv:2505.09598 · IEA 2024 · EEA)";
    let _ = writeln!(buf, "│ {title:<inner$} │");
    let _ = writeln!(buf, "├{dashes}┤");
    let energy = format!("Energy        {:>10.2} kWh", c.energy_kwh);
    let co2 = format!(
        "CO₂           {:>10.2} kg    (grid {:.3} kg/kWh)",
        c.co2_kg, GRID_KG_PER_KWH
    );
    let trees = format!(
        "Tree-years    {:>10.2}       ({:.0} tree-days @ {:.1} g/day)",
        c.tree_years, c.tree_days, TREE_G_PER_DAY
    );
    let _ = writeln!(buf, "│ {energy:<inner$} │");
    let _ = writeln!(buf, "│ {co2:<inner$} │");
    let _ = writeln!(buf, "│ {trees:<inner$} │");
    let _ = writeln!(buf, "└{dashes}┘");
}
