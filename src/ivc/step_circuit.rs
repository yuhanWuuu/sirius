use ff::PrimeField;
use halo2_proofs::{
    circuit::{AssignedCell, Layouter},
    plonk::ConstraintSystem,
};
use halo2curves::CurveAffine;

use crate::{plonk::RelaxedPlonkInstance, poseidon::ROTrait};

#[derive(Debug, thiserror::Error)]
pub enum SynthesisError {
    #[error(transparent)]
    Halo2(#[from] halo2_proofs::plonk::Error),
}

/// The `StepCircuit` trait represents a step in incremental computation in
/// Incrementally Verifiable Computation (IVC).
///
/// # Overview
/// - The trait is crucial for representing a computation step within an IVC
///   framework.
/// - It provides an abstraction for handling inputs and outputs efficiently
///   at each computation step.
/// - The trait should be implemented by circuits that are intended to represent
///   a step in incremental computation.
///
/// # API
/// Design based on [`halo2_proofs::plonk::Circuit`] and
/// [`nova::traits::circuit`](https://github.com/microsoft/Nova/blob/main/src/traits/circuit.rs#L7)
///
/// # `const ARITY: usize`
/// The number of inputs or outputs of each step. `synthesize` and `output`
/// methods are expected to take as input a vector of size equal to
/// arity and output a vector of size equal to arity.
///
/// # References
/// - For a detailed understanding of IVC and the context in which a trait
///   `StepCircuit` might be used, refer to the 'Section 5' of
///   [Nova Whitepaper](https://eprint.iacr.org/2023/969.pdf).
pub trait StepCircuit<const ARITY: usize, F: PrimeField> {
    type StepConfig: Clone;

    /// Configure the step circuit. This method initializes necessary
    /// fixed columns and advice columns, but does not create any instance
    /// columns.
    ///
    /// This setup is crucial for the functioning of the IVC-based system.
    fn configure(cs: &mut ConstraintSystem<F>) -> Self::StepConfig;

    /// Sythesize the circuit for a computation step and return variable
    /// that corresponds to the output of the step z_{i+1}
    /// this method will be called when we synthesize the IVC_Circuit
    ///
    /// Return `z_out` result
    fn synthesize(
        &self,
        config: Self::StepConfig,
        layouter: &mut impl Layouter<F>,
        z_in: &[AssignedCell<F, F>; ARITY],
    ) -> Result<[AssignedCell<F, F>; ARITY], SynthesisError>;

    /// An auxiliary function that allows you to perform a calculation step
    /// without using ConstraintSystem.
    ///
    /// By default, performs the step with a dummy `ConstraintSystem`
    fn output(&self, z_in: &[F; ARITY]) -> [F; ARITY] {
        todo!(
            "Default impl with `Self::synthesize` wrap
            and comment about when manual impl needed by {z_in:?}"
        )
    }
}

#[derive(Debug)]
pub(crate) enum ConfigureError {
    InstanceColumnNotAllowed,
}

/// The private expanding trait that checks that no instance columns have
/// been created during [`StepCircuit::configure`].
///
/// IVC Circuit should use this method.
pub(crate) trait ConfigureWithInstanceCheck<const ARITY: usize, F: PrimeField>:
    StepCircuit<ARITY, F>
{
    fn configure_with_instance_check(
        cs: &mut ConstraintSystem<F>,
    ) -> Result<<Self as StepCircuit<ARITY, F>>::StepConfig, ConfigureError>;
}

impl<const A: usize, F: PrimeField, C: StepCircuit<A, F>> ConfigureWithInstanceCheck<A, F> for C {
    fn configure_with_instance_check(
        cs: &mut ConstraintSystem<F>,
    ) -> Result<<Self as StepCircuit<A, F>>::StepConfig, ConfigureError> {
        let before = cs.num_instance_columns();

        let config = <Self as StepCircuit<A, F>>::configure(cs);

        if before == cs.num_instance_columns() {
            Ok(config)
        } else {
            Err(ConfigureError::InstanceColumnNotAllowed)
        }
    }
}

// TODO Rename
pub struct SynthesizeStepParams<G: CurveAffine, RO: ROTrait<G>> {
    pub limb_width: usize,
    pub n_limbs: usize,
    /// A boolean indicating if this is the primary circuit
    pub is_primary_circuit: bool,
    pub ro_constant: RO::Constants,
}

pub struct StepInputs<'link, const ARITY: usize, G: CurveAffine, RO: ROTrait<G>> {
    params: &'link SynthesizeStepParams<G, RO>,
    step: G::Base,

    z_0: [AssignedCell<G::Scalar, G::Scalar>; ARITY],
    z_in: [AssignedCell<G::Scalar, G::Scalar>; ARITY],

    // TODO docs
    U: Option<RelaxedPlonkInstance<G>>,

    // TODO docs
    u: Option<RelaxedPlonkInstance<G>>,

    // TODO docs
    T_commitment: Option<G::Scalar>,
}

// TODO
/// Extends a step circuit so that it can be used inside an IVC
///
/// This trait functionality is equivalent to structure `NovaAugmentedCircuit` from nova codebase
pub(crate) trait StepCircuitExt<'link, const ARITY: usize, G: CurveAffine>:
    StepCircuit<ARITY, G::Scalar>
{
    fn synthesize_step<RO: ROTrait<G>>(
        &self,
        _config: <Self as StepCircuit<ARITY, G::Scalar>>::StepConfig,
        _layouter: &mut impl Layouter<G::Scalar>,
        _input: StepInputs<ARITY, G, RO>,
    ) -> Result<[AssignedCell<G::Scalar, G::Scalar>; ARITY], SynthesisError> {
        todo!()
    }

    fn synthesize_step_base_case<RO: ROTrait<G>>(
        &self,
        _config: <Self as StepCircuit<ARITY, G::Scalar>>::StepConfig,
        _layouter: &mut impl Layouter<G::Scalar>,
        _input: StepInputs<ARITY, G, RO>,
    ) -> Result<[AssignedCell<G::Scalar, G::Scalar>; ARITY], SynthesisError> {
        todo!()
    }

    fn synthesize_step_not_base_case<RO: ROTrait<G>>(
        &self,
        _config: <Self as StepCircuit<ARITY, G::Scalar>>::StepConfig,
        _layouter: &mut impl Layouter<G::Scalar>,
        _input: StepInputs<ARITY, G, RO>,
    ) -> Result<[AssignedCell<G::Scalar, G::Scalar>; ARITY], SynthesisError> {
        todo!()
    }
}

// auto-impl for all `StepCircuit` trait `StepCircuitExt`
impl<'link, const ARITY: usize, G: CurveAffine, SP: StepCircuit<ARITY, G::Scalar>>
    StepCircuitExt<'link, ARITY, G> for SP
{
}