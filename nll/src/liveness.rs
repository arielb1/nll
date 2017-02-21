use graph::{BasicBlockIndex, FuncGraph};
use env::{Environment, Point};
use graph_algorithms::Graph;
use graph_algorithms::bit_set::{BitBuf, BitSet, BitSlice};
use nll_repr::repr;
use std::collections::HashMap;

/// Compute the set of live variables at each point.
pub struct Liveness {
    var_bits: HashMap<repr::Variable, usize>,
    liveness: BitSet<FuncGraph>,
}

impl Liveness {
    pub fn new(env: &Environment) -> Liveness {
        let var_bits: HashMap<_, _> = env.graph.decls()
                                               .iter()
                                               .cloned()
                                               .zip(0..)
                                               .collect();
        let liveness = BitSet::new(env.graph, var_bits.len());
        let mut this = Liveness { var_bits, liveness };
        this.compute(env);
        this
    }

    pub fn live_on_entry(&self, v: repr::Variable, b: BasicBlockIndex) -> bool {
        let bit = self.var_bits[&v];
        self.liveness.bits(b).get(bit)
    }

    fn compute(&mut self, env: &Environment) {
        let mut bits = self.liveness.empty_buf();
        let mut changed = true;
        while changed {
            changed = false;

            for &block in &env.reverse_post_order {
                self.simulate_block(env, &mut bits, block, |_p, _a, _s| ());
                changed |= self.liveness.insert_bits_from_slice(block, bits.as_slice());
            }
        }
    }

    fn simulate_block<CB>(&mut self,
                          env: &Environment,
                          buf: &mut BitBuf,
                          block: BasicBlockIndex,
                          mut callback: CB)
        where CB: FnMut(Point, &repr::Action, BitSlice)
    {
        buf.clear();

        // everything live in a successor is live at the exit of the block
        for succ in env.graph.successors(block) {
            buf.set_from(self.liveness.bits(succ));
        }

        // walk backwards through the actions
        for (index, action) in env.graph.block_data(block).actions.iter().enumerate().rev() {
            let (def_var, use_var) = action.def_use();

            // anything we write to is no longer live
            for v in def_var {
                buf.kill(self.var_bits[&v]);
            }

            // anything we read from, we make live
            for v in use_var {
                buf.set(self.var_bits[&v]);
            }

            let point = Point { block, action: index };
            callback(point, action, buf.as_slice());
        }
    }
}

trait UseDefs {
    fn def_use(&self) -> (Vec<repr::Variable>, Vec<repr::Variable>);
}

impl UseDefs for repr::Action {
    fn def_use(&self) -> (Vec<repr::Variable>, Vec<repr::Variable>) {
        match *self {
            repr::Action::Borrow(v) => (vec!(v), vec!()),
            repr::Action::Assign(l, r) => (vec!(l), vec![r]),
            repr::Action::Subtype(a, b) => (vec!(), vec![a, b]),
            repr::Action::Use(v) => (vec!(), vec!(v)),
            repr::Action::Write(v) => (vec!(), vec!(v)),
            repr::Action::Noop => (vec!(), vec!()),
        }
    }
}
