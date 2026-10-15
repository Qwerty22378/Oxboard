//! Offline render mode: run a plan for a number of blocks with no devices and no clock, and
//! collect the master bus. For tests and tools; it calls the same interpreter as the mix thread.

use crate::Frame;
use crate::graph::plan::RenderPlan;
use crate::rt::mix_thread::render_block;

/// Render `blocks` blocks and return the master bus, interleaved.
pub fn render(plan: &mut RenderPlan, blocks: usize) -> Vec<Frame> {
    let mut out = Vec::with_capacity(blocks * plan.block());
    for _ in 0..blocks {
        let [l, r] = render_block(plan, 0);
        out.extend(l.iter().zip(r).map(|(l, r)| [*l, *r]));
    }
    out
}
