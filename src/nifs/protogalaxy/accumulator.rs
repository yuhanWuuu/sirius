use std::iter;

use crate::{
    ff::Field,
    halo2curves::CurveAffine,
    plonk::{self, PlonkInstance, PlonkTrace},
    poseidon::{AbsorbInRO, ROTrait},
    util,
};

/// Represents an accumulator for folding multiple instances into a single instance,
/// following the accumulation schemes.
pub struct Accumulator<C: CurveAffine> {
    /// `φ`: Represents the combined state of all instances & witnesses. It is a summary that
    /// captures the essential data and relationships from the instances being merged.
    pub(super) trace: PlonkTrace<C>,

    /// `β`: A random value used in the folding process. It helps ensure the unique
    /// and secure combination of instances, preventing manipulation.
    pub(super) betas: Box<[C::ScalarExt]>,

    /// `e`: an accumulated value that encapsulates the result of the folding operation. it serves
    /// as a concise representation of the correctness and properties of the folded instances.
    pub(super) e: C::ScalarExt,
}

impl<C: CurveAffine, RO: ROTrait<C::Base>> AbsorbInRO<C::Base, RO> for Accumulator<C> {
    fn absorb_into(&self, ro: &mut RO) {
        ro.absorb(&self.trace.u).absorb_field_iter(
            self.betas
                .iter()
                .chain(iter::once(&self.e))
                .map(|b| util::fe_to_fe::<C::ScalarExt, C::Base>(b).unwrap()),
        );
    }
}

pub type AccumulatorArgs = plonk::PlonkTraceArgs;

impl<C: CurveAffine> Accumulator<C> {
    pub fn new(args: AccumulatorArgs, count_of_evaluation: usize) -> Self {
        Self {
            betas: vec![C::ScalarExt::ZERO; count_of_evaluation].into_boxed_slice(),
            e: C::ScalarExt::ZERO,
            trace: PlonkTrace::new(args),
        }
    }
}

/// Represents an accumulator for folding multiple instances into a single instance,
/// following the accumulation schemes.
pub struct AccumulatorInstance<C: CurveAffine> {
    /// `φ`: Represents the combined state of all instances. It is a summary that captures the
    /// essential data and relationships from the instances being merged.
    pub(super) ins: PlonkInstance<C>,

    /// `β`: A random value used in the folding process. It helps ensure the unique
    /// and secure combination of instances, preventing manipulation.
    pub(super) betas: Box<[C::ScalarExt]>,

    /// `e`: an accumulated value that encapsulates the result of the folding operation. it serves
    /// as a concise representation of the correctness and properties of the folded instances.
    pub(super) e: C::ScalarExt,
}

impl<C: CurveAffine> From<Accumulator<C>> for AccumulatorInstance<C> {
    fn from(value: Accumulator<C>) -> Self {
        let Accumulator {
            betas,
            trace: PlonkTrace { u: ins, w: _ },
            e,
        } = value;

        AccumulatorInstance { betas, ins, e }
    }
}

impl<C: CurveAffine, RO: ROTrait<C::Base>> AbsorbInRO<C::Base, RO> for AccumulatorInstance<C> {
    fn absorb_into(&self, ro: &mut RO) {
        ro.absorb(&self.ins).absorb_field_iter(
            self.betas
                .iter()
                .chain(iter::once(&self.e))
                .map(|b| util::fe_to_fe::<C::ScalarExt, C::Base>(b).unwrap()),
        );
    }
}