use std::{
    iter,
    num::NonZeroUsize,
    ops::{Add, Mul},
};

use itertools::*;
use tracing::*;

use crate::{
    ff::PrimeField,
    fft,
    group::ff::WithSmallOrderMulGroup,
    plonk::{self, eval, GetChallenges, GetWitness, PlonkStructure},
    polynomial::{expression::QueryIndexContext, lagrange, univariate::UnivariatePoly},
    util::TryMultiProduct,
};

mod folded_witness;
pub(crate) use folded_witness::FoldedWitness;

#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum Error {
    #[error(transparent)]
    Eval(#[from] eval::Error),
    #[error("You can't fold 0 traces")]
    EmptyTracesNotAllowed,
}

/// This function calculates F(X), which mathematically looks like this:
///
/// $$F(X)=\sum_{i=0}^{n-1}pow_{i}(\boldsymbol{\beta}+X\cdot\boldsymbol{\delta})f_i(w)$$
///
/// - `f_i` - iteratively all gates for all rows sequentially. The order is taken from
///           [`plonk::iter_evaluate_witness`].
/// - `pow_i` - `i` degree of challenge
///
/// # Algorithm
///
/// We use [`Itertools::tree_reduce`] & create `points_count` iterators for `pow_i`, where each
/// iterator uses a different challenge (`X`) from the cyclic group, and then iterate over all
/// these iterators at once.
///
/// I.e. item `i` from this iterator is a collection of [pow_i(X0), pow_i(X1), ...]
///
/// f₀  f₁ f₂  f₃  f₄  f₅  f₆  f₇
/// │   │  │   │   │   │   │   │
/// 1   β  1   β   1   β   1   β
/// │   │  │   │   │   │   │   │
/// └───f₀₁└───f₂₃ └───f₄₅ └───f₆₇
///     │      │       │       │
///     1      β²      1       β²
///     │      │       │       │
///     └──────f₀₁₂₃   └───────f₄₅₆₇
///            │               │
///            1               β⁴
///            │               │
///            └───────────────f₀₁₂₃₄₅₆₇
///
/// Each β here is a vector of all `X`, and each node except leaves contains all counted
/// Each `f` here is fₙₘ =  fₙ * 1 + fₘ * βⁱ
///
/// # Note
///
/// Unlike [`compute_G`] where `X` challenge affects the nodes of the tree and generates multiple
/// values from them, here multiple values are generated by edges, and they are stored everywhere
/// except leaves.
#[instrument(skip_all)]
pub(crate) fn compute_F<F: PrimeField>(
    ctx: &PolyContext<'_, F>,
    betas: impl Iterator<Item = F>,
    delta: F,
    trace: &(impl Sync + GetChallenges<F> + GetWitness<F>),
) -> Result<UnivariatePoly<F>, Error> {
    // `n` in paper
    let Some(count_of_evaluation) = get_count_of_valuation_with_padding(ctx.S) else {
        return Ok(UnivariatePoly::new_zeroed(0));
    };

    // `t` in paper
    let fft_points_count_F = ctx.fft_points_count_F();

    debug!(
        "count_of_evaluation: {count_of_evaluation};
        points_count: {fft_points_count_F}"
    );

    // Use the elements of the cyclic group together with beta & delta as challenge and calculate them
    // degrees
    //
    // Since we are using a tree-based algorithm, we need `{X^1, X^2, ..., X^{log2(n)}}` of all
    // challenges.
    //
    // Even for large `count_of_evaluation` this will be a small number, so we can
    // collect it
    let betas = betas.take(ctx.betas_count()).collect::<Box<[_]>>();
    assert_eq!(betas.len(), ctx.betas_count());
    let deltas = iter::successors(Some(delta), |d| Some(d.pow([2])))
        .take(ctx.betas_count())
        .collect::<Box<[_]>>();
    debug!("betas & deltas ready");

    let challenges_powers = lagrange::iter_cyclic_subgroup::<F>(fft_points_count_F.ilog2())
        .map(|X| {
            betas
                .iter()
                .zip_eq(deltas.iter())
                .map(|(beta, delta)| *beta + (X * delta))
                .collect::<Box<_>>()
        })
        .collect::<Box<[_]>>();
    debug!("challenges powers ready ready");

    /// Auxiliary wrapper for using the tree to evaluate polynomials
    #[derive(Debug)]
    enum Node<F: PrimeField> {
        Leaf(F),
        Calculated {
            /// Intermediate results for all calculated challenges
            /// Every point calculated for specific challenge
            points: Box<[F]>,
            /// Node height relative to leaf height
            height: NonZeroUsize,
        },
    }

    let evaluated = plonk::iter_evaluate_witness::<F>(ctx.S, trace)
        .chain(iter::repeat(Ok(F::ZERO)))
        .take(count_of_evaluation.get())
        .map(|result_with_evaluated_gate| {
            debug!("witness row: {:?}", result_with_evaluated_gate);
            result_with_evaluated_gate.map(Node::Leaf)
        })
        // TODO #324 Migrate to a parallel algorithm
        // TODO #324 Implement `try_tree_reduce` to stop on the first error
        .tree_reduce(|left_w, right_w| {
            let (left_w, right_w) = (left_w?, right_w?);

            match (left_w, right_w) {
                (Node::Leaf(left), Node::Leaf(right)) => Ok(Node::Calculated {
                    points: challenges_powers
                        .iter()
                        .map(|challenge_powers| left + (right * challenge_powers[0]))
                        .collect(),
                    height: NonZeroUsize::new(1).unwrap(),
                }),
                (
                    Node::Calculated {
                        points: mut left,
                        height: l_height,
                    },
                    Node::Calculated {
                        points: right,
                        height: r_height,
                    },
                    // The tree must be binary, so we only calculate at the one node level
                ) if l_height.eq(&r_height) => {
                    itertools::multizip((challenges_powers.iter(), left.iter_mut(), right.iter()))
                        .for_each(|(challenge_powers, left, right)| {
                            *left += *right * challenge_powers[l_height.get()]
                        });

                    Ok(Node::Calculated {
                        points: left,
                        height: l_height.saturating_add(1),
                    })
                }
                other => unreachable!("this case must be unreachable: {other:?}"),
            }
        });

    match evaluated {
        Some(Ok(Node::Calculated { mut points, .. })) => {
            fft::ifft(&mut points);
            Ok(UnivariatePoly(points))
        }
        Some(Err(err)) => Err(err.into()),
        other => unreachable!("this case must be unreachable: {other:?}"),
    }
}

pub struct PolyContext<'s, F: PrimeField> {
    S: &'s PlonkStructure<F>,
    /// Equal to the number of incoming traces plus one (accumulator)
    /// Must be a power of two
    instances_to_fold: usize,
    /// The number of points used in G(X)
    ///
    /// Used in [`compute_G`]
    ///
    /// Equal to `(traces_len * max_gate_degree + 1).next_power_of_two()`
    fft_points_count_G: usize,
    /// Number of calculations, padding with zeros to the nearest power of two
    count_of_evaluation_with_padding: usize,
}

impl<'s, F: PrimeField> PolyContext<'s, F> {
    pub fn new(
        S: &'s PlonkStructure<F>,
        traces: &[(impl Sync + GetChallenges<F> + GetWitness<F>)],
    ) -> Self {
        let count_of_evaluation = get_count_of_valuation_with_padding(S).unwrap().get();

        let instances_to_fold = traces.len() + 1;
        assert!(instances_to_fold.is_power_of_two());

        let fft_points_count_G = get_points_count(S, traces.len());

        Self {
            S,
            instances_to_fold,
            fft_points_count_G,
            count_of_evaluation_with_padding: count_of_evaluation,
        }
    }

    pub fn betas_count(&self) -> usize {
        self.count_of_evaluation_with_padding.ilog2() as usize
    }

    pub fn fft_points_count_F(&self) -> usize {
        (self.betas_count() + 1).next_power_of_two()
    }

    pub fn fft_log_domain_size_G(&self) -> u32 {
        self.fft_points_count_G.ilog2()
    }

    pub fn lagrange_domain(&self) -> u32 {
        self.instances_to_fold.ilog2()
    }

    pub fn get_lagrange_domain<const TRACES_LEN: usize>() -> u32 {
        let instances_to_fold = TRACES_LEN + 1;
        assert!(instances_to_fold.is_power_of_two());
        instances_to_fold.ilog2()
    }

    pub fn fft_log_domain_size_K(&self) -> u32 {
        self.fft_points_count_G
            .add(1)
            .saturating_sub(self.instances_to_fold)
            .next_power_of_two() as u32
    }
}

/// This function calculates G(X), which mathematically looks like this:
///
/// $$G(X)=\sum_{i=0}^{n-1}\operatorname{pow}_i(\boldsymbol{\beta}+\alpha\cdot\boldsymbol{\delta})f_i(L_0(X)w+\sum_{j\in[k]}L_j(X)w_j)$$
///
/// - `f_i` - iteratively all gates for all rows sequentially. The order is taken from
///           [`plonk::iter_evaluate_witness`].
/// - `pow_i` - `i` degree of challenge
/// - `L` - lagrange poly
///
/// # Algorithm
///
/// We use [`Itertools::tree_reduce`] & store in each node `X` points, for each X challenge
///
/// I.e. item `i` from this iterator is a collection of [pow_i(X0), pow_i(X1), ...]
///
/// f₀  f₁ f₂  f₃  f₄  f₅  f₆  f₇
/// │   │  │   │   │   │   │   │
/// 1   β' 1   β'  1   β'  1   β'
/// │   │  │   │   │   │   │   │
/// └───f₀₁└───f₂₃ └───f₄₅ └───f₆₇
///     │      │       │       │
///     1      β'₂     1       β'₂
///     │      │       │       │
///     └──────f₀₁₂₃   └───────f₄₅₆₇
///            │               │
///            1               β'₄
///            │               │
///            └───────────────f₀₁₂₃₄₅₆₇
///
/// Where β'ᵢ= βⁱ + (α * δⁱ)
/// Each `f` here is vector (leafs too) of elements for each challenge with: fₙₘ =  fₙ * 1 + fₘ * β'ᵢ
///
/// # Note
///
/// Unlike [`compute_F`] where `X` challenge affects the edges of the tree, here the set of values
/// is in the nodes
#[instrument(skip_all)]
pub(crate) fn compute_G<F: PrimeField>(
    ctx: &PolyContext<F>,
    betas_stroke: impl Iterator<Item = F>,
    accumulator: &(impl Sync + GetChallenges<F> + GetWitness<F>),
    traces: &[(impl Sync + GetChallenges<F> + GetWitness<F>)],
) -> Result<UnivariatePoly<F>, Error> {
    if traces.is_empty() {
        return Err(Error::EmptyTracesNotAllowed);
    }

    let betas_stroke = betas_stroke.take(ctx.betas_count()).collect::<Box<[_]>>();
    assert_eq!(ctx.betas_count(), betas_stroke.len());

    let points_for_fft = lagrange::iter_cyclic_subgroup(ctx.fft_log_domain_size_G())
        .take(ctx.fft_points_count_G)
        .collect::<Box<[_]>>();

    /// Auxiliary wrapper for using the tree to evaluate polynomials
    #[derive(Debug)]
    struct Node<F: PrimeField> {
        values: Box<[F]>,
        height: usize,
    }

    let evaluated =
        FoldedWitness::new(&points_for_fft, ctx.lagrange_domain(), accumulator, traces)
        .iter() // folded witness iter per each X
        .map(|folded_trace| plonk::iter_evaluate_witness::<F>(ctx.S, folded_trace)
            .chain(iter::repeat(Ok(F::ZERO)))
            .take(ctx.count_of_evaluation_with_padding)
        )
        .try_multi_product()
        .map(|points| points.map(|points| Node { values: points, height: 0 }))
        .tree_reduce(|left, right| {
            let (
                Node {
                    values: mut left,
                    height: l_height,
                },
                Node {
                    values: right,
                    height: r_height,
                },
            ) = (left?, right?);

            if l_height.eq(&r_height) {
                left.iter_mut().zip(right.iter()).for_each(|(left, right)| {
                    *left += *right * betas_stroke[l_height];
                });

                Ok(Node {
                    values: left,
                    height: l_height.saturating_add(1),
                })
            } else {
                unreachable!("different heights should not be here because the tree is binary: {l_height} != {r_height}")
            }
        });

    match evaluated {
        Some(Ok(Node {
            values: mut points, ..
        })) => {
            fft::ifft(&mut points);
            Ok(UnivariatePoly(points))
        }
        Some(Err(err)) => Err(err.into()),
        other => unreachable!("this case must be unreachable: {other:?}"),
    }
}

#[derive(Clone)]
pub(crate) struct PolyChallenges<F> {
    pub(crate) betas: Box<[F]>,
    pub(crate) alpha: F,
    pub(crate) delta: F,
}

#[derive(Clone)]
pub(crate) struct BetaStrokeIter<F> {
    cha: PolyChallenges<F>,
    beta_index: usize,
}

impl<F> PolyChallenges<F> {
    pub(crate) fn iter_beta_stroke(self) -> BetaStrokeIter<F> {
        BetaStrokeIter {
            cha: self,
            beta_index: 0,
        }
    }
}

impl<F: Clone + Mul<Output = F> + Add<Output = F>> Iterator for BetaStrokeIter<F> {
    type Item = F;

    /// `next = beta[i] + (alpha * delta^{2^i})`
    fn next(&mut self) -> Option<Self::Item> {
        let next = self.cha.betas.get(self.beta_index).cloned()?
            + (self.cha.alpha.clone() * self.cha.delta.clone());

        self.beta_index += 1;
        self.cha.delta = self.cha.delta.clone().mul(self.cha.delta.clone());

        Some(next)
    }
}

pub(crate) fn compute_K<F: WithSmallOrderMulGroup<3>>(
    ctx: &PolyContext<F>,
    poly_F_in_alpha: F,
    betas_stroke: impl Iterator<Item = F>,
    accumulator: &(impl Sync + GetChallenges<F> + GetWitness<F>),
    traces: &[(impl Sync + GetChallenges<F> + GetWitness<F>)],
) -> Result<UnivariatePoly<F>, Error> {
    let poly_G = compute_G(ctx, betas_stroke, accumulator, traces)?;
    Ok(compute_K_from_G(ctx, poly_G, poly_F_in_alpha))
}

fn compute_K_from_G<F: WithSmallOrderMulGroup<3>>(
    ctx: &PolyContext<F>,
    poly_G: UnivariatePoly<F>,
    poly_F_in_alpha: F,
) -> UnivariatePoly<F> {
    UnivariatePoly::coset_ifft(
        lagrange::iter_cyclic_subgroup::<F>(ctx.fft_log_domain_size_K())
            .map(|X| F::ZETA * X)
            // TODO #293
            //.zip(poly_G.coset_fft())
            //.map(|(X, poly_G_in_X)| {
            .map(|X| {
                let poly_G_in_X = poly_G.eval(X);

                let poly_L0_in_X =
                    lagrange::iter_eval_lagrange_poly_for_cyclic_group(X, ctx.lagrange_domain())
                        .next()
                        .unwrap();

                // Z(X) == 0, for X in coset_cyclic_subgroup
                let poly_Z_in_X = lagrange::eval_vanish_polynomial(ctx.instances_to_fold, X);

                let poly_K_in_X = (poly_G_in_X - (poly_F_in_alpha * poly_L0_in_X))
                    * poly_Z_in_X.invert().expect("Z(X) must be not equal to 0");

                assert_eq!(
                    (poly_F_in_alpha * poly_L0_in_X) + (poly_Z_in_X * poly_K_in_X),
                    poly_G_in_X
                );

                poly_K_in_X
            })
            .collect::<Box<[_]>>(),
    )
}

pub fn get_count_of_valuation<F: PrimeField>(S: &PlonkStructure<F>) -> Option<NonZeroUsize> {
    let count_of_rows = 2usize.pow(S.k as u32);
    let count_of_gates = S.gates.len();

    NonZeroUsize::new(count_of_rows * count_of_gates)
}

fn get_count_of_valuation_with_padding<F: PrimeField>(
    S: &PlonkStructure<F>,
) -> Option<NonZeroUsize> {
    get_count_of_valuation(S).and_then(|v| v.checked_next_power_of_two())
}

fn get_points_count<F: PrimeField>(S: &PlonkStructure<F>, traces_len: usize) -> usize {
    let ctx = QueryIndexContext::from(S);
    let max_degree = S
        .gates
        .iter()
        .map(|poly| poly.degree(&ctx))
        .max()
        .unwrap_or_default();

    (traces_len * max_degree + 1).next_power_of_two()
}

#[cfg(test)]
mod test {
    use std::iter;

    use bitter::{BitReader, LittleEndianReader};
    use halo2_proofs::{halo2curves::ff::PrimeField, plonk::Circuit};
    use tracing::*;
    use tracing_test::traced_test;

    use super::{folded_witness::FoldedWitness, PolyContext};
    use crate::{
        commitment::CommitmentKey,
        ff::Field as _Field,
        halo2curves::{bn256, CurveAffine},
        plonk::{self, test_eval_witness::poseidon_circuit, PlonkStructure, PlonkTrace},
        polynomial::{lagrange, univariate::UnivariatePoly},
        poseidon::{
            random_oracle::{self, ROTrait},
            PoseidonRO, Spec,
        },
        table::CircuitRunner,
    };

    type Curve = bn256::G1Affine;
    type Field = <Curve as CurveAffine>::ScalarExt;

    /// Spec for off-circuit poseidon
    const POSEIDON_PERMUTATION_WIDTH: usize = 3;
    const POSEIDON_RATE: usize = POSEIDON_PERMUTATION_WIDTH - 1;

    const R_F1: usize = 4;
    const R_P1: usize = 3;
    pub type PoseidonSpec =
        Spec<<Curve as CurveAffine>::Base, POSEIDON_PERMUTATION_WIDTH, POSEIDON_RATE>;

    type RO = <PoseidonRO<POSEIDON_PERMUTATION_WIDTH, POSEIDON_RATE> as random_oracle::ROPair<
        <Curve as CurveAffine>::Base,
    >>::OffCircuit;

    fn get_trace(
        k_table_size: u32,
        circuit: impl Circuit<Field>,
        instances: Vec<Vec<Field>>,
    ) -> (PlonkStructure<Field>, PlonkTrace<Curve>) {
        let runner = CircuitRunner::<Field, _>::new(k_table_size, circuit, vec![]);

        let S = runner.try_collect_plonk_structure().unwrap();
        debug!("plonk collected");
        let witness = runner.try_collect_witness().unwrap();
        debug!("witness collected");

        let key = CommitmentKey::setup(18, b"");
        debug!("key generated");
        let PlonkTrace { u, w } = S
            .run_sps_protocol(
                &key,
                &instances,
                &witness,
                &mut RO::new(PoseidonSpec::new(R_F1, R_P1)),
            )
            .unwrap();

        (S, PlonkTrace { u, w })
    }

    fn poseidon_trace() -> (PlonkStructure<Field>, PlonkTrace<Curve>) {
        get_trace(
            13,
            poseidon_circuit::TestPoseidonCircuit::<_>::default(),
            vec![vec![Field::from(4097)]],
        )
    }

    fn pow_i<'l, F: PrimeField>(
        i: usize,
        t: usize,
        challenges_powers: impl Iterator<Item = &'l F>,
    ) -> F {
        let bytes = i.to_le_bytes();
        let mut reader = LittleEndianReader::new(&bytes);

        iter::repeat_with(|| reader.read_bit().unwrap_or(false))
            .zip(challenges_powers)
            .map(|(b_j, beta_in_2j)| match b_j {
                true => *beta_in_2j,
                false => F::ONE,
            })
            .take(t)
            .reduce(|acc, coeff| acc * coeff)
            .unwrap()
    }

    #[traced_test]
    #[test]
    fn cmp_with_direct_eval_of_F() {
        let (S, mut trace) = poseidon_trace();

        let mut rnd = rand::thread_rng();
        let mut gen = iter::repeat_with(|| Field::random(&mut rnd));

        trace.w.W.iter_mut().for_each(|row| {
            row.iter_mut()
                .for_each(|v| *v = gen.by_ref().next().unwrap())
        });

        let traces = [trace];
        let ctx = PolyContext::new(&S, &traces);

        let delta = gen.by_ref().next().unwrap();
        let betas = gen.by_ref().take(ctx.betas_count()).collect::<Box<[_]>>();

        let evaluated_poly_F =
            super::compute_F(&ctx, betas.iter().copied(), delta, &traces[0]).unwrap();

        lagrange::iter_cyclic_subgroup::<Field>(ctx.fft_points_count_F().ilog2())
            .chain(gen.take(10))
            .for_each(|X| {
                let challenge_vector = betas
                    .iter()
                    .zip(iter::successors(Some(delta), |d| Some(d.pow([2]))))
                    .take(ctx.count_of_evaluation_with_padding)
                    .map(|(beta, delta)| beta + (X * delta))
                    .collect::<Box<[_]>>();

                let result_with_direct_algo = plonk::iter_evaluate_witness::<Field>(&S, &traces[0])
                    .enumerate()
                    .map(|(index, f_i)| {
                        pow_i(
                            index,
                            ctx.count_of_evaluation_with_padding,
                            challenge_vector.iter(),
                        ) * f_i.unwrap()
                    })
                    .sum();

                assert_eq!(
                    evaluated_poly_F.eval(X),
                    result_with_direct_algo,
                    "not match for {X:?}"
                );
            })
    }

    #[traced_test]
    #[test]
    fn cmp_with_direct_eval_of_G() {
        let (S, trace) = poseidon_trace();
        let mut rnd = rand::thread_rng();
        let mut gen = iter::repeat_with(|| Field::random(&mut rnd));

        let traces = iter::repeat_with(|| {
            let mut trace = trace.clone();
            trace
                .w
                .W
                .iter_mut()
                .for_each(|row| row.iter_mut().zip(gen.by_ref()).for_each(|(v, r)| *v = r));
            trace
        })
        .take(3)
        .collect::<Box<[_]>>();

        let ctx = PolyContext::new(&S, &traces);

        let beta_stroke = gen.by_ref().take(ctx.betas_count()).collect::<Box<[_]>>();

        let accumulator = trace;
        let evaluated_poly_G =
            super::compute_G(&ctx, beta_stroke.iter().copied(), &accumulator, &traces).unwrap();

        let points_for_fft =
            lagrange::iter_cyclic_subgroup(ctx.fft_log_domain_size_G()).collect::<Box<[_]>>();

        FoldedWitness::new(
            &points_for_fft,
            ctx.lagrange_domain(),
            &accumulator,
            &traces,
        )
        .iter()
        .map(|folded_trace| {
            plonk::iter_evaluate_witness::<Field>(&S, folded_trace)
                .chain(iter::repeat(Ok(Field::ZERO)))
                .take(ctx.count_of_evaluation_with_padding)
        })
        .zip(points_for_fft.iter().copied().chain(gen.take(10)))
        .for_each(|(folded_witness, X)| {
            let result_with_direct_algo = folded_witness
                .enumerate()
                .map(|(index, f_i)| {
                    pow_i(
                        index,
                        ctx.count_of_evaluation_with_padding,
                        beta_stroke.iter(),
                    ) * f_i.unwrap()
                })
                .sum();

            assert_eq!(
                evaluated_poly_G.eval(X),
                result_with_direct_algo,
                "for {X:?}"
            );
        });
    }

    pub fn vanish_poly<F: PrimeField>(degree: usize) -> UnivariatePoly<F> {
        let mut coeff = vec![F::ZERO; degree].into_boxed_slice();
        coeff[0] = -F::ONE;
        coeff[degree - 1] = F::ONE;
        UnivariatePoly(coeff)
    }

    #[traced_test]
    #[test]
    fn zero_f() {
        debug!("start");
        let (S, trace) = poseidon_trace();
        debug!("trace ready");
        let mut rnd = rand::thread_rng();

        let delta = Field::random(&mut rnd);

        let traces = [trace];

        debug!("start compute F");
        assert!(super::compute_F(
            &super::PolyContext::new(&S, &traces),
            iter::repeat_with(move || Field::random(&mut rnd)),
            delta,
            &traces[0],
        )
        .unwrap()
        .iter()
        .all(|f| f.is_zero().into()));
    }

    #[traced_test]
    #[test]
    fn non_zero_f() {
        let (S, mut trace) = poseidon_trace();
        let mut rnd = rand::thread_rng();
        trace
            .w
            .W
            .iter_mut()
            .for_each(|row| row.iter_mut().for_each(|el| *el = Field::random(&mut rnd)));

        let delta = Field::random(&mut rnd);

        let traces = [trace];

        assert_ne!(
            super::compute_F(
                &super::PolyContext::new(&S, &traces),
                iter::repeat_with(|| Field::random(&mut rnd)),
                delta,
                &traces[0],
            ),
            Ok(UnivariatePoly::from_iter(
                iter::repeat(Field::ZERO).take(16)
            ))
        );
    }

    #[traced_test]
    #[test]
    fn zero_g() {
        let (S, trace) = poseidon_trace();
        let mut rnd = rand::thread_rng();

        let traces = [trace];
        assert!(super::compute_G(
            &super::PolyContext::new(&S, &traces),
            iter::repeat_with(|| Field::random(&mut rnd)),
            &traces[0].clone(),
            &traces
        )
        .unwrap()
        .iter()
        .all(|f| f.is_zero().into()));
    }

    #[traced_test]
    #[test]
    fn non_zero_g() {
        let (S, mut trace) = poseidon_trace();
        let mut rnd = rand::thread_rng();
        trace
            .w
            .W
            .iter_mut()
            .for_each(|row| row.iter_mut().for_each(|el| *el = Field::random(&mut rnd)));

        let traces = [trace];
        assert_ne!(
            super::compute_G(
                &super::PolyContext::new(&S, &traces),
                iter::repeat_with(|| Field::random(&mut rnd)),
                &traces[0].clone(),
                &traces
            ),
            Ok(UnivariatePoly::from_iter(
                iter::repeat(Field::ZERO).take(16)
            ))
        );
    }
}
