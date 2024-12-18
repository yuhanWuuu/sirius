use halo2_proofs::arithmetic::CurveAffine;

use crate::{
    commitment,
    constants::NUM_CHALLENGE_BITS,
    plonk::{eval::Error as EvalError, PlonkInstance},
    poseidon::ROTrait,
    util::ScalarToBase,
};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Error {
    #[error(transparent)]
    Eval(#[from] EvalError),
    #[error("Sps verification fail challenge not match at index {challenge_index}")]
    ChallengeNotMatch { challenge_index: usize },
    #[error("For this challenges count table must have lookup aguments")]
    LackOfLookupArguments,
    #[error("Lack of advices, should call `TableData::assembly` first")]
    LackOfAdvices,
    #[error("Only 0..=3 num of challenges supported: {challenges_count} not")]
    UnsupportedChallengesCount { challenges_count: usize },
    #[error("Error while commit {annotation} with err: {err:?}")]
    WrongCommitmentSize {
        annotation: &'static str,
        err: commitment::Error,
    },
}

/// This trait verifies whether the instance is faithly generated by a Special soundness protocol (sps)
/// Reference: section 3.1 of [protostar](https://eprint.iacr.org/2023/620)
pub trait SpecialSoundnessVerifier<C: CurveAffine, RO: ROTrait<C::Base>> {
    fn sps_verify(&self, ro_nark: &mut RO) -> Result<(), Error>;
}

impl<C: CurveAffine, RO: ROTrait<C::Base>> SpecialSoundnessVerifier<C, RO> for PlonkInstance<C> {
    fn sps_verify(&self, ro_nark: &mut RO) -> Result<(), Error> {
        let num_challenges = self.challenges.len();

        if num_challenges == 0 {
            return Ok(());
        }

        ro_nark.absorb_field_iter(
            self.instances
                .iter()
                .flat_map(|inst| inst.iter())
                .map(|val| C::scalar_to_base(val).unwrap()),
        );

        for i in 0..num_challenges {
            if ro_nark
                .absorb_point(&self.W_commitments[i])
                .squeeze::<C>(NUM_CHALLENGE_BITS)
                .ne(&self.challenges[i])
            {
                return Err(Error::ChallengeNotMatch { challenge_index: i });
            }
        }
        Ok(())
    }
}
