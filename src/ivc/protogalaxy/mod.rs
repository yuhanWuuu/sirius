mod verify_chip {
    use std::iter;

    use itertools::Itertools;
    use tracing::*;

    use crate::{
        gadgets::ecc::AssignedPoint,
        halo2_proofs::{
            arithmetic::Field,
            circuit::{AssignedCell, Chip, Value as Halo2Value},
            halo2curves::{
                ff::{FromUniformBytes, PrimeField, PrimeFieldBits},
                CurveAffine,
            },
            plonk::Error as Halo2PlonkError,
        },
        main_gate::{
            AdviceCyclicAssignor, AssignedValue, MainGate, MainGateConfig, RegionCtx, WrapValue,
        },
        nifs::protogalaxy::{
            self,
            poly::{PolyChallenges, PolyContext},
        },
        plonk::PlonkInstance,
        polynomial::{lagrange::iter_cyclic_subgroup, univariate::UnivariatePoly},
        poseidon::ROCircuitTrait,
        util::ScalarToBase,
    };

    #[derive(Debug, thiserror::Error)]
    pub enum Error {
        #[error("Error while assign {annotation}: {err:?}")]
        Assign {
            annotation: &'static str,
            err: Halo2PlonkError,
        },

        #[error("Error while squeeze challenges: {err:?}")]
        Squeeze { err: Halo2PlonkError },

        #[error("Error while calculate deltas: {err:?}")]
        Deltas { err: Halo2PlonkError },

        #[error("Error while calculate beta stoke: {err:?}")]
        BetasStroke { err: Halo2PlonkError },

        #[error("Error while calculate new `e`: {err:?}")]
        WhileE { err: Halo2PlonkError },

        #[error("Error while fold instancess: {err:?}")]
        Fold { err: Halo2PlonkError },

        #[allow(clippy::upper_case_acronyms)]
        #[error("SPS Verify Error: {err:?}")]
        SPS { err: Halo2PlonkError },
    }

    /// Assigned version of [`crate::plonk::PlonkInstance`]
    pub struct AssignedPlonkInstance<C: CurveAffine> {
        W_commitments: Vec<AssignedPoint<C>>,
        instances: Vec<Vec<AssignedValue<C::Base>>>,
        challenges: Vec<AssignedValue<C::Base>>,
    }

    impl<C: CurveAffine> AssignedPlonkInstance<C> {
        pub fn assign<const T: usize>(
            region: &mut RegionCtx<C::Base>,
            main_gate_config: MainGateConfig<T>,
            pi: PlonkInstance<C>,
        ) -> Result<Self, Error> {
            let PlonkInstance {
                W_commitments,
                instances,
                challenges,
            } = pi;

            let mut assigner = main_gate_config.advice_cycle_assigner();

            let W_commitments = W_commitments
                .iter()
                .enumerate()
                .map(|(i, W_commitment)| {
                    assigner.assign_next_advice_point(
                        region,
                        || format!("W_commitments[{i}]"),
                        W_commitment,
                    )
                })
                .collect::<Result<Vec<_>, _>>();

            let instances = instances
                .iter()
                .map(|instance| {
                    assigner.assign_all_advice(
                        region,
                        || "instance",
                        instance.iter().map(|i| C::scalar_to_base(i).unwrap()),
                    )
                })
                .collect::<Result<Vec<_>, _>>();

            let challenges = assigner.assign_all_advice(
                region,
                || "challenges",
                challenges.iter().map(|i| C::scalar_to_base(i).unwrap()),
            );

            let map_err = |err| Error::Assign {
                annotation: "PlonkInstance",
                err,
            };

            Ok(Self {
                W_commitments: W_commitments.map_err(map_err)?,
                instances: instances.map_err(map_err)?,
                challenges: challenges.map_err(map_err)?,
            })
        }

        pub fn iter_wrap_value(&self) -> impl '_ + Iterator<Item = WrapValue<C::Base>> {
            let Self {
                W_commitments,
                instances,
                challenges,
            } = self;

            W_commitments
                .iter()
                .flat_map(|W_commitment| WrapValue::from_assigned_point(W_commitment).into_iter())
                .chain(instances.iter().flat_map(|instance| {
                    instance
                        .iter()
                        .map(|value| WrapValue::Assigned(value.clone()))
                }))
                .chain(
                    challenges
                        .iter()
                        .map(|challenge| WrapValue::Assigned(challenge.clone())),
                )
        }
    }

    /// Assigned version of [`crate::nifs::protogalaxy::accumulator::AccumulatorInstance`]
    pub struct AssignedAccumulatorInstance<C: CurveAffine> {
        ins: AssignedPlonkInstance<C>,
        betas: Box<[AssignedValue<C::Base>]>,
        e: AssignedValue<C::Base>,
    }

    impl<C: CurveAffine> AssignedAccumulatorInstance<C> {
        pub fn assign<const T: usize>(
            region: &mut RegionCtx<C::Base>,
            main_gate_config: MainGateConfig<T>,
            acc: protogalaxy::AccumulatorInstance<C>,
        ) -> Result<Self, Error> {
            let protogalaxy::AccumulatorInstance { ins, betas, e } = acc;

            let ins = AssignedPlonkInstance::assign(region, main_gate_config.clone(), ins)?;

            let mut assigner = main_gate_config.advice_cycle_assigner();

            // Convert and assign `betas`
            let betas = betas
                .iter()
                .map(|beta| {
                    assigner.assign_next_advice(
                        region,
                        || "beta",
                        C::scalar_to_base(beta).unwrap(), // assuming conversion to base field is needed
                    )
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| Error::Assign {
                    annotation: "AccumulatorInstance::betas",
                    err,
                })?
                .into_boxed_slice();

            // Convert and assign `e`
            let e = assigner
                .assign_next_advice(region, || "e", C::scalar_to_base(&e).unwrap())
                .map_err(|err| Error::Assign {
                    annotation: "AccumulatorInstance::e",
                    err,
                })?;

            Ok(Self { ins, betas, e })
        }

        pub fn iter_wrap_value(&self) -> impl '_ + Iterator<Item = WrapValue<C::Base>> {
            let Self { ins, betas, e } = self;

            ins.iter_wrap_value()
                .chain(betas.iter().map(|beta| WrapValue::Assigned(beta.clone())))
                .chain(iter::once(WrapValue::Assigned(e.clone())))
        }
    }

    /// Powers of one assigned value counted on-circuit
    ///
    /// Since powers are required many times during synthesis, let's keep these values separate
    /// ```math
    /// x^0, x^1, x^2, x^3, ... x^i, ...
    /// ```
    pub struct ValuePowers<F: PrimeField> {
        powers: Vec<AssignedValue<F>>,
    }

    impl<F: PrimeField> ValuePowers<F> {
        pub fn new(one: AssignedValue<F>, value: AssignedValue<F>) -> Self {
            if let Some(one) = one.value().unwrap() {
                assert_eq!(one, &F::ONE);
            }

            Self {
                powers: vec![one, value],
            }
        }

        pub fn iter(&self) -> impl Iterator<Item = &AssignedValue<F>> {
            self.powers.iter()
        }

        pub fn value(&self) -> AssignedValue<F> {
            self.powers
                .get(1)
                .expect("Cannot be created without at least one element inside")
                .clone()
        }

        /// Get from cache or calculate the `exp` degree of original value
        ///
        /// `self.value^exp`
        ///
        /// TODO: Can be improved by using two mult in main_gate
        pub fn get_or_eval<const T: usize>(
            &mut self,
            region: &mut RegionCtx<F>,
            main_gate: &MainGate<F, T>,
            exp: usize,
        ) -> Result<AssignedValue<F>, Halo2PlonkError> {
            if let Some(value) = self.powers.get(exp) {
                return Ok(value.clone());
            }

            while self.powers.len() <= exp {
                let value = self.value();
                let last = self.powers.last().unwrap();
                let new = main_gate.mul(region, &value, last)?;
                self.powers.push(new);
            }

            Ok(self.powers.get(exp).cloned().unwrap())
        }
    }

    /// Assigned version of [`crate::polynomial::univariate::UnivariatePoly`]
    pub struct AssignedUnivariatePoly<F: PrimeField>(UnivariatePoly<AssignedValue<F>>);

    impl<F: PrimeField> AssignedUnivariatePoly<F> {
        pub fn assign<const T: usize>(
            region: &mut RegionCtx<F>,
            main_gate_config: MainGateConfig<T>,
            annotation: &'static str,
            poly: &UnivariatePoly<F>,
        ) -> Result<Self, Error> {
            let up = AssignedUnivariatePoly(UnivariatePoly(
                main_gate_config
                    .advice_cycle_assigner()
                    .assign_all_advice(region, || annotation, poly.iter().copied())
                    .map_err(|err| Error::Assign { annotation, err })?
                    .into_boxed_slice(),
            ));

            region.next();

            Ok(up)
        }

        pub fn iter_wrap_value(&self) -> impl '_ + Iterator<Item = WrapValue<F>> {
            debug!("iter wrap value len: {:?}", self.0.len());

            self.0
                .iter()
                .inspect(|coeff| debug!("coeff {coeff:?}"))
                .map(|coeff| WrapValue::Assigned(coeff.clone()))
        }

        fn degree(&self) -> usize {
            self.0.len()
        }

        fn len(&self) -> usize {
            self.0.len()
        }

        pub fn eval<const T: usize>(
            &self,
            region: &mut RegionCtx<F>,
            main_gate: &MainGate<F, T>,
            challenge_powers: &mut ValuePowers<F>,
        ) -> Result<AssignedValue<F>, Halo2PlonkError> {
            let main_gate_config = main_gate.config();

            let enable_selectors = |region: &mut RegionCtx<F>| {
                [
                    main_gate_config.q_m[0],
                    main_gate_config.q_m[1],
                    main_gate_config.q_i,
                    main_gate_config.q_o,
                ]
                .iter()
                .try_for_each(|col| region.assign_fixed(|| "one", *col, F::ZERO).map(|_| ()))
            };
            let coeffs_col = [main_gate_config.state[0], main_gate_config.state[2]];
            let cha_col = [main_gate_config.state[1], main_gate_config.state[3]];
            let prev_col = &main_gate_config.input;
            let result_col = &main_gate_config.out;

            challenge_powers.get_or_eval(region, main_gate, self.len().saturating_sub(1))?;

            self.0
                .iter()
                .zip_eq(challenge_powers.iter())
                .chunks(2)
                .into_iter()
                .try_fold(Option::<AssignedValue<F>>::None, |prev, chunks| {
                    let (coeffs, cha_in_power): (Vec<_>, Vec<_>) = chunks.unzip();
                    enable_selectors(region)?;

                    let assigned_prev = match prev {
                        None => {
                            region.assign_advice(|| "zero", *prev_col, Halo2Value::known(F::ZERO))
                        }
                        Some(prev_cell) => region.assign_advice_from(
                            || "previous chunk values",
                            *prev_col,
                            prev_cell,
                        ),
                    }?;

                    let assigned_coeffs = coeffs
                        .iter()
                        .zip_eq(coeffs_col)
                        .map(|(coeff, col)| region.assign_advice_from(|| "coeff", col, *coeff))
                        .collect::<Result<Box<[_]>, _>>()?;

                    let assigned_cha = cha_in_power
                        .iter()
                        .zip_eq(cha_col)
                        .map(|(cha_in_power, col)| {
                            region.assign_advice_from(|| "cha", col, *cha_in_power)
                        })
                        .collect::<Result<Box<[_]>, _>>()?;

                    let output = assigned_coeffs
                        .iter()
                        .zip_eq(assigned_cha.iter())
                        .fold(assigned_prev.value().copied(), |res, (coeff, cha)| {
                            res + (coeff.value().copied() * cha.value())
                        });

                    let assigned_output = region.assign_advice(|| "result", *result_col, output);

                    debug!(
                        "coeffs: {:?}; cha_in_power: {:?}, prev: {:?}, output: {:?}",
                        coeffs.iter().map(|cell| cell.value()).collect::<Box<[_]>>(),
                        cha_in_power
                            .iter()
                            .map(|cell| cell.value())
                            .collect::<Box<[_]>>(),
                        assigned_prev.value(),
                        assigned_output
                            .as_ref()
                            .ok()
                            .and_then(|cell| cell.value().unwrap()),
                    );

                    region.next();

                    assigned_output.map(Some)
                })?
                .ok_or(Halo2PlonkError::Synthesis)
        }
    }

    /// Assigned version of [`crate::nifs::protogalaxy::Proof]
    pub struct AssignedProof<F: PrimeField> {
        poly_F: AssignedUnivariatePoly<F>,
        poly_K: AssignedUnivariatePoly<F>,
    }

    impl<F: PrimeField> AssignedProof<F> {
        pub fn assign<const T: usize>(
            region: &mut RegionCtx<F>,
            main_gate_config: MainGateConfig<T>,
            proof: protogalaxy::Proof<F>,
        ) -> Result<Self, Error> {
            let protogalaxy::Proof { poly_K, poly_F } = proof;

            debug!(
                "poly F len is {}, poly K len is {}",
                poly_F.len(),
                poly_K.len()
            );

            Ok(Self {
                poly_F: AssignedUnivariatePoly::assign::<T>(
                    region,
                    main_gate_config.clone(),
                    "poly_F",
                    &poly_F,
                )?,
                poly_K: AssignedUnivariatePoly::assign::<T>(
                    region,
                    main_gate_config.clone(),
                    "poly_K",
                    &poly_K,
                )?,
            })
        }
    }

    /// Assigned version of [`crate::nifs::protogalaxy::VerifierParam`]
    pub struct AssignedVerifierParam<C: CurveAffine> {
        pp_digest: AssignedPoint<C>,
    }

    impl<C: CurveAffine> AssignedVerifierParam<C> {
        pub fn assign<const T: usize>(
            region: &mut RegionCtx<C::Base>,
            main_gate_config: MainGateConfig<T>,
            vp: &protogalaxy::VerifierParam<C>,
        ) -> Result<Self, Error> {
            let protogalaxy::VerifierParam { pp_digest } = vp;

            Ok(Self {
                pp_digest: main_gate_config
                    .advice_cycle_assigner::<C::Base>()
                    .assign_next_advice_point(region, || "pp_digest", pp_digest)
                    .map_err(|err| Error::Assign {
                        annotation: "VerifierParam",
                        err,
                    })?,
            })
        }
    }

    /// Assigned version of [`crate::nifs::protogalaxy::Challenges`]
    struct AssignedChallanges<F: PrimeField> {
        delta: AssignedValue<F>,
        alpha: AssignedValue<F>,
        gamma: AssignedValue<F>,
    }

    impl<F: PrimeField> AssignedChallanges<F> {
        #[instrument(skip_all, name = "on_circuit_generate")]
        fn generate<C: CurveAffine<Base = F>>(
            region: &mut RegionCtx<C::Base>,
            mut ro_circuit: impl ROCircuitTrait<C::Base>,
            vp: AssignedVerifierParam<C>,
            accumulator: &AssignedAccumulatorInstance<C>,
            incoming: &[AssignedPlonkInstance<C>],
            proof: &AssignedProof<C::Base>,
        ) -> Result<AssignedChallanges<F>, Halo2PlonkError>
        where
            C::Base: FromUniformBytes<64> + PrimeFieldBits,
            C::ScalarExt: FromUniformBytes<64> + PrimeFieldBits,
        {
            let delta = ro_circuit
                .absorb_point(WrapValue::from_assigned_point(&vp.pp_digest))
                .absorb_iter(accumulator.iter_wrap_value())
                .absorb_iter(incoming.iter().flat_map(|tr| tr.iter_wrap_value()))
                .squeeze(region)?;

            let alpha = ro_circuit
                .absorb_iter(proof.poly_F.iter_wrap_value())
                .squeeze(region)?;

            let gamma = ro_circuit
                .absorb_iter(proof.poly_K.iter_wrap_value())
                .squeeze(region)?;

            Ok(AssignedChallanges {
                delta,
                alpha,
                gamma,
            })
        }
    }

    /// Calculate v, v^2, v^4, v^8 ...
    fn calculate_exponentiation_sequence<F: PrimeField, const T: usize>(
        region: &mut RegionCtx<F>,
        main_gate: &MainGate<F, T>,
        value: AssignedCell<F, F>,
        len: usize,
    ) -> Result<Box<[AssignedCell<F, F>]>, Halo2PlonkError> {
        iter::successors(
            Some(Ok(value)),
            |prev| -> Option<Result<AssignedCell<F, F>, Halo2PlonkError>> {
                let prev = match prev {
                    Ok(val) => val,
                    Err(_err) => {
                        return None;
                    }
                };

                Some(main_gate.mul(region, prev, prev))
            },
        )
        .take(len)
        .collect::<Result<Box<[_]>, Halo2PlonkError>>()
    }

    fn calculate_betas_stroke<C: CurveAffine, const T: usize>(
        region: &mut RegionCtx<C::Base>,
        main_gate: &MainGate<C::Base, T>,
        cha: PolyChallenges<AssignedCell<C::Base, C::Base>>,
    ) -> Result<Box<[AssignedCell<C::Base, C::Base>]>, Error> {
        let deltas =
            calculate_exponentiation_sequence(region, main_gate, cha.delta, cha.betas.len())
                .map_err(|err| Error::Deltas { err })?;

        cha.betas
            .iter()
            .zip_eq(deltas)
            .map(|(beta, delta_power)| {
                let alpha_mul_delta = main_gate.mul(region, &cha.alpha, &delta_power)?;
                main_gate.add(region, beta, &alpha_mul_delta)
            })
            .collect::<Result<Box<[_]>, Halo2PlonkError>>()
            .map_err(|err| Error::BetasStroke { err })
    }

    /// Evaluate the values of the Lagrange polynomial for a cyclic subgroup of length `n` (`2.pow(log_n)`) at
    /// the `challenge` point
    ///
    /// You can look at [`fft::get_omega_or_inv`] to see how the target cyclic group is formed
    ///
    /// # Mathematical Representation
    ///
    /// ```math
    /// L_i(X)=\frac{\omega^i}{n}\frac{X^n-1}{X-\omega^i}
    /// ```
    /// where {1, \omega, \omega^2, ..., \omega^n} - cyclic group, check [`iter_cyclic_subgroup`] for
    /// more details
    ///
    /// # Generics
    /// `T` is setup for main gate
    /// - `L`: 'Length' - constant representing the number of instances to
    ///                   fold in a single `prove`. `L-1` be power of two
    fn eval_lagrange_poly<F: PrimeField, const T: usize, const L: usize>(
        region: &mut RegionCtx<F>,
        main_gate: &MainGate<F, T>,
        lagrange_index: usize,
        cha: &mut ValuePowers<F>,
    ) -> Result<AssignedValue<F>, Halo2PlonkError> {
        let lagrange_domain = PolyContext::<F>::get_lagrange_domain::<L>();
        let points_count = 2usize.pow(lagrange_domain);
        assert!(lagrange_index < points_count);

        let inverted_n = F::from_u128(points_count as u128)
            .invert()
            .expect("safe because it's `2^log_n`");
        let value = iter_cyclic_subgroup::<F>(lagrange_domain)
            .nth(lagrange_index)
            .unwrap();

        let X = cha.value();

        let X_sub_value = main_gate.add_with_const(region, &X, -value)?;

        let (is_zero_X_sub_value, X_sub_value_inverted) =
            main_gate.invert_with_flag(region, X_sub_value)?;

        let X_pow_n = cha.get_or_eval(region, main_gate, points_count)?;
        let X_pow_n_sub_1 = main_gate.add_with_const(region, &X_pow_n, -F::ONE)?;

        let is_zero_X_pow_n_sub_1 = main_gate.is_zero_term(region, X_pow_n_sub_1.clone())?;

        let is_numerator_denominator_zero =
            main_gate.mul(region, &is_zero_X_sub_value, &is_zero_X_pow_n_sub_1)?;

        let lhs = main_gate.mul(region, &X_pow_n_sub_1, &X_sub_value_inverted)?;
        let fractional = main_gate.mul_by_const(region, &lhs, value * inverted_n)?;

        let one = cha.get_or_eval(region, main_gate, 0)?;

        main_gate.conditional_select(region, &one, &fractional, &is_numerator_denominator_zero)
    }

    /// This fn calculates vanishing polynomial $Z(X)$ from the formula $G(X)=F(\alpha)L_0(X)+K(X)Z(X)$
    /// # Parameters
    /// - `log_n` - logarithm of polynomial degree
    /// - `point` - `x` - eval Lagrange polynomials at this point
    /// # Result - x^n - 1
    /// X^{2^log_n} - 1
    /// -1 * X^0 + 0 * X^1 + ... + a * X^{2^log_n}
    fn eval_vanish_polynomial<F: PrimeField, const T: usize>(
        region: &mut RegionCtx<F>,
        main_gate: &MainGate<F, T>,
        degree: usize,
        cha: &mut ValuePowers<F>,
    ) -> Result<AssignedValue<F>, Halo2PlonkError> {
        let cha_in_degree = cha.get_or_eval(region, main_gate, degree)?;
        main_gate.add_with_const(region, &cha_in_degree, -F::ONE)
    }

    // F(alpha) * L(gamma) + Z(gamma) * K(gamma)
    fn calculate_e<F: PrimeField, const T: usize, const L: usize>(
        region: &mut RegionCtx<F>,
        main_gate: &MainGate<F, T>,
        proof: &AssignedProof<F>,
        gamma_cha: &mut ValuePowers<F>,
        alpha_cha: &mut ValuePowers<F>,
    ) -> Result<AssignedValue<F>, Halo2PlonkError> {
        let lagrange_domain = PolyContext::<F>::get_lagrange_domain::<L>();

        let poly_L0_in_gamma = eval_lagrange_poly::<F, T, L>(region, main_gate, 0, gamma_cha)?;

        let poly_F_alpha = proof.poly_F.eval(region, main_gate, alpha_cha)?;
        let poly_Z_gamma =
            eval_vanish_polynomial(region, main_gate, 1 << lagrange_domain, gamma_cha)?;
        let poly_K_gamma = proof.poly_K.eval(region, main_gate, gamma_cha)?;

        let lhs = main_gate.mul(region, &poly_F_alpha, &poly_L0_in_gamma)?;
        let rhs = main_gate.mul(region, &poly_Z_gamma, &poly_K_gamma)?;

        main_gate.add(region, &lhs, &rhs)
    }

    /// Fold instances, but without on-circuit ecc operations
    fn fold_instances<C: CurveAffine, const T: usize, const L: usize>(
        region: &mut RegionCtx<C::Base>,
        main_gate: &MainGate<C::Base, T>,
        acc: &AssignedPlonkInstance<C>,
        incoming: &[AssignedPlonkInstance<C>; L],
        gamma_cha: &mut ValuePowers<C::Base>,
    ) -> Result<AssignedPlonkInstance<C>, Halo2PlonkError> {
        let l_0 = eval_lagrange_poly::<C::Base, T, L>(region, main_gate, 0, gamma_cha)?;

        let new_acc = AssignedPlonkInstance {
            W_commitments: acc.W_commitments.clone(), // Don't fold here, delegate it to secondary circuit
            instances: acc
                .instances
                .iter()
                .map(|instance| {
                    instance
                        .iter()
                        .map(|cell| main_gate.mul(region, cell, &l_0))
                        .collect::<Result<Vec<_>, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?,
            challenges: acc
                .challenges
                .iter()
                .map(|cell| main_gate.mul(region, cell, &l_0))
                .collect::<Result<Vec<_>, _>>()?,
        };

        incoming
            .iter()
            .enumerate()
            .try_fold(new_acc, |mut acc, (index, tr)| {
                let l_n =
                    eval_lagrange_poly::<C::Base, T, L>(region, main_gate, index + 1, gamma_cha)?;

                acc.instances
                    .iter_mut()
                    .zip_eq(tr.instances.iter())
                    .try_for_each(|(acc_instances, instances)| {
                        acc_instances.iter_mut().zip_eq(instances).try_for_each(
                            |(acc_instance, instance)| {
                                let rhs = main_gate.mul(region, instance, &l_n)?;

                                let new = main_gate.add(region, acc_instance, &rhs)?;

                                *acc_instance = new;

                                Result::<_, Halo2PlonkError>::Ok(())
                            },
                        )
                    })?;

                acc.challenges
                    .iter_mut()
                    .zip_eq(tr.challenges.iter())
                    .try_for_each(|(acc_challenge, challenge)| {
                        let rhs = main_gate.mul(region, challenge, &l_n)?;

                        let new = main_gate.add(region, acc_challenge, &rhs)?;

                        *acc_challenge = new;

                        Result::<_, Halo2PlonkError>::Ok(())
                    })?;

                Result::<_, Halo2PlonkError>::Ok(acc)
            })
    }

    pub fn verify_sps<C: CurveAffine, const L: usize>(
        region: &mut RegionCtx<C::Base>,
        ro_circuit: &mut impl ROCircuitTrait<C::Base>,
        incoming: &[AssignedPlonkInstance<C>; L],
    ) -> Result<(), Halo2PlonkError>
    where
        C::Base: FromUniformBytes<64> + PrimeFieldBits,
        C::ScalarExt: FromUniformBytes<64> + PrimeFieldBits,
    {
        for pi in incoming {
            if pi.challenges.is_empty() {
                continue;
            }

            ro_circuit.absorb_iter(pi.instances.iter().flat_map(|inst| inst.iter()));

            for (W_commitment, challenge) in pi.W_commitments.iter().zip_eq(pi.challenges.iter()) {
                let expected = ro_circuit
                    .absorb_point(WrapValue::from_assigned_point(W_commitment))
                    .squeeze(region)?;

                region.constrain_equal(expected.cell(), challenge.cell())?;
            }
        }

        Ok(())
    }

    /// Assigned version of `fn verify` logic from [`crate::nifs::protogalaxy::ProtoGalaxy`].
    ///
    /// # Algorithm
    ///
    /// The logic of the proof generation follows several key steps:
    ///
    /// 1. **Generate Delta:**
    ///     - **RO Seeds**: includes all input parameters except `ck`
    ///     - `delta = ro_acc.squeeze()`
    ///
    /// 2. **Generate Alpha:**
    ///     - **RO Update**: absorb `proof.poly_F`
    ///     - `alpha = ro_acc.squeeze()`
    ///
    /// 3. **Update Beta* Values:**
    ///     - `beta*[i] = beta[i] + alpha * delta[i]`
    ///
    /// 4. **Generate Gamma:**
    ///     - **RO Update**: Absorb `proof.poly_K`
    ///     - `gamma = ro_acc.squeeze()`
    ///
    /// 5. **Fold the Instance:**
    ///     - [`ProtoGalaxy::fold_instance`]
    pub fn verify<C: CurveAffine, const L: usize, const T: usize>(
        region: &mut RegionCtx<C::Base>,
        main_gate_config: MainGateConfig<T>,
        ro_circuit: impl ROCircuitTrait<C::Base>,
        vp: AssignedVerifierParam<C>,
        accumulator: AssignedAccumulatorInstance<C>,
        incoming: &[AssignedPlonkInstance<C>; L],
        proof: AssignedProof<C::Base>,
    ) -> Result<AssignedAccumulatorInstance<C>, Error>
    where
        C::Base: FromUniformBytes<64> + PrimeFieldBits,
        C::ScalarExt: FromUniformBytes<64> + PrimeFieldBits,
    {
        let AssignedChallanges {
            delta,
            alpha,
            gamma,
        } = AssignedChallanges::generate(region, ro_circuit, vp, &accumulator, incoming, &proof)
            .map_err(|err| Error::Squeeze { err })?;

        let main_gate = MainGate::new(main_gate_config);

        let betas = calculate_betas_stroke::<C, T>(
            region,
            &main_gate,
            PolyChallenges {
                betas: accumulator.betas.clone(),
                alpha: alpha.clone(),
                delta,
            },
        )?;

        let one = region
            .assign_advice(
                || "one",
                main_gate.config().state[0],
                Halo2Value::known(C::Base::ONE),
            )
            .map_err(|err| Error::Assign {
                annotation: "one",
                err,
            })?;
        region.next();

        let mut gamma_powers = ValuePowers::new(one.clone(), gamma);
        let mut alpha_powers = ValuePowers::new(one, alpha);

        let e = calculate_e::<C::Base, T, L>(
            region,
            &main_gate,
            &proof,
            &mut gamma_powers,
            &mut alpha_powers,
        )
        .map_err(|err| Error::WhileE { err })?;

        let ins = fold_instances(
            region,
            &main_gate,
            &accumulator.ins,
            incoming,
            &mut gamma_powers,
        )
        .map_err(|err| Error::Fold { err })?;

        Ok(AssignedAccumulatorInstance { ins, betas, e })
    }

    #[cfg(test)]
    mod tests {
        use tracing_test::traced_test;

        use super::*;
        use crate::{
            halo2_proofs::{
                arithmetic::Field,
                circuit::{
                    floor_planner::single_pass::SingleChipLayouter, Chip, Layouter,
                    SimpleFloorPlanner,
                },
                dev::MockProver,
                plonk::{Circuit, ConstraintSystem},
            },
            halo2curves::{bn256::G1Affine as Affine, group::prime::PrimeCurveAffine},
            main_gate::MainGate,
            nifs::{
                self,
                protogalaxy::{AccumulatorArgs, VerifierParam},
            },
            polynomial,
            poseidon::{poseidon_circuit::PoseidonChip, PoseidonHash, ROTrait, Spec},
            table::WitnessCollector,
        };

        const T: usize = 5;
        const RATE: usize = T - 1;
        const K: usize = 14;

        type Base = <Affine as CurveAffine>::Base;

        fn get_witness_collector() -> (WitnessCollector<Base>, MainGateConfig<T>) {
            let mut cs = ConstraintSystem::default();
            let config = MainGate::<Base, T>::configure(&mut cs);
            let witness = WitnessCollector {
                instances: vec![vec![]],
                advice: vec![vec![Base::ZERO.into(); 1 << K]; cs.num_advice_columns()],
            };

            (witness, config)
        }

        struct Mock {
            params: VerifierParam<Affine>,
            spec: Spec<<Affine as CurveAffine>::Base, T, RATE>,
            acc: nifs::protogalaxy::Accumulator<Affine>,
            proof: nifs::protogalaxy::Proof<<Affine as CurveAffine>::ScalarExt>,
        }

        impl Mock {
            fn new() -> Self {
                let params = VerifierParam::<Affine> {
                    pp_digest: Affine::identity(),
                };

                let spec = Spec::<<Affine as CurveAffine>::Base, 5, 4>::new(10, 10);

                let acc = nifs::protogalaxy::Accumulator::<Affine>::new(
                    AccumulatorArgs {
                        num_io: Box::new([]),
                        num_challenges: 0,
                        num_witness: 0,
                        k_table_size: K,
                        round_sizes: Box::new([]),
                    },
                    10,
                );

                let mut values = (0..).map(Into::into);
                let proof = nifs::protogalaxy::Proof {
                    poly_F: UnivariatePoly::from_iter(values.by_ref().take(10)),
                    poly_K: UnivariatePoly::from_iter(values.take(10)),
                };

                Self {
                    params,
                    spec,
                    acc,
                    proof,
                }
            }
        }

        #[traced_test]
        #[test]
        fn challanges() {
            let m = Mock::new();

            let off_circuit_challenges = nifs::protogalaxy::Challenges::generate(
                &m.params,
                &mut PoseidonHash::new(m.spec.clone()),
                &m.acc,
                iter::empty::<&PlonkInstance<Affine>>(),
                &m.proof,
            );

            let (mut wc, config) = get_witness_collector();

            let mut layouter = SingleChipLayouter::new(&mut wc, vec![]).unwrap();

            let on_circuit_challanges = layouter
                .assign_region(
                    || "challenges_test",
                    move |region| {
                        let mut region = RegionCtx::new(region, 0);

                        let Mock {
                            params,
                            spec,
                            acc,
                            proof,
                        } = &m;

                        let params =
                            AssignedVerifierParam::assign::<T>(&mut region, config.clone(), params)
                                .unwrap();
                        let acc = AssignedAccumulatorInstance::assign(
                            &mut region,
                            config.clone(),
                            acc.clone().into(),
                        )
                        .unwrap();

                        let proof = AssignedProof::assign(
                            &mut region,
                            config.clone(),
                            protogalaxy::Proof {
                                poly_F: proof.poly_F.fe_to_fe().unwrap(),
                                poly_K: proof.poly_K.fe_to_fe().unwrap(),
                            },
                        )
                        .unwrap();

                        AssignedChallanges::generate(
                            &mut region,
                            PoseidonChip::new(config.clone(), spec.clone()),
                            params,
                            &acc,
                            &[],
                            &proof,
                        )
                    },
                )
                .unwrap();

            assert_eq!(
                on_circuit_challanges.delta.value().unwrap(),
                Some(&crate::util::fe_to_fe(&off_circuit_challenges.delta).unwrap()),
                "delta(1) on-circuit vs off-circuit",
            );

            assert_eq!(
                on_circuit_challanges.alpha.value().unwrap(),
                Some(&crate::util::fe_to_fe(&off_circuit_challenges.alpha).unwrap()),
                "alpha(2) on-circuit vs off-circuit",
            );

            assert_eq!(
                on_circuit_challanges.gamma.value().unwrap(),
                Some(&crate::util::fe_to_fe(&off_circuit_challenges.gamma).unwrap()),
                "gamma(3) on-circuit vs off-circuit",
            );
        }

        #[traced_test]
        #[test]
        fn betas_stroke() {
            let mut rnd = rand::thread_rng();
            let mut rnd = iter::repeat_with(|| Base::random(&mut rnd));

            let cha = PolyChallenges {
                alpha: rnd.next().unwrap(),
                delta: rnd.next().unwrap(),
                betas: rnd.take(10).collect(),
            };

            fn assign_poly_challenges<F: PrimeField, const T: usize>(
                region: &mut RegionCtx<F>,
                main_gate_config: MainGateConfig<T>,
                cha: &PolyChallenges<F>,
            ) -> Result<PolyChallenges<AssignedCell<F, F>>, Halo2PlonkError> {
                let mut assigner = main_gate_config.advice_cycle_assigner();

                let PolyChallenges {
                    betas,
                    alpha,
                    delta,
                } = cha;

                Ok(PolyChallenges {
                    betas: assigner
                        .assign_all_advice(region, || "betas", betas.iter().copied())?
                        .into_boxed_slice(),
                    alpha: assigner.assign_next_advice(region, || "alpha", *alpha)?,
                    delta: assigner.assign_next_advice(region, || "delta", *delta)?,
                })
            }

            let off_circuit_beta_strokes = cha.clone().iter_beta_stroke().collect::<Box<[_]>>();

            let (mut wc, main_gate_config) = get_witness_collector();

            let mut layouter = SingleChipLayouter::new(&mut wc, vec![]).unwrap();

            let on_circuit_beta_strokes = layouter
                .assign_region(
                    || "betas_stroke",
                    move |region| {
                        let mut region = RegionCtx::new(region, 0);
                        let cha =
                            assign_poly_challenges(&mut region, main_gate_config.clone(), &cha)
                                .unwrap();
                        let main_gate = MainGate::<Base, T>::new(main_gate_config.clone());

                        Ok(
                            calculate_betas_stroke::<Affine, T>(&mut region, &main_gate, cha)
                                .unwrap(),
                        )
                    },
                )
                .unwrap()
                .iter()
                .map(|cell| *cell.value().unwrap().unwrap())
                .collect::<Box<[_]>>();

            assert_eq!(off_circuit_beta_strokes, on_circuit_beta_strokes);
        }

        #[traced_test]
        #[test]
        fn poly_eval() {
            struct TestCircuit;

            impl Circuit<Base> for TestCircuit {
                type Config = MainGateConfig<T>;
                type FloorPlanner = SimpleFloorPlanner;

                fn without_witnesses(&self) -> Self {
                    todo!()
                }

                fn configure(meta: &mut ConstraintSystem<Base>) -> Self::Config {
                    MainGate::configure(meta)
                }

                fn synthesize(
                    &self,
                    main_gate_config: Self::Config,
                    mut layouter: impl Layouter<Base>,
                ) -> Result<(), Halo2PlonkError> {
                    let cha = Base::from_u128(123);
                    let poly = UnivariatePoly::from_iter((0..).map(Into::into).take(10));

                    let off_circuit_res = poly.eval(cha);

                    let on_circuit_res = layouter.assign_region(
                        || "assigned_poly_eval",
                        move |region| {
                            let mut region = RegionCtx::new(region, 0);

                            let cha = region
                                .assign_advice(
                                    || "",
                                    main_gate_config.state[0],
                                    Halo2Value::known(cha),
                                )
                                .unwrap();

                            let one = region
                                .assign_advice(
                                    || "",
                                    main_gate_config.state[1],
                                    Halo2Value::known(Base::ONE),
                                )
                                .unwrap();

                            region.next();

                            let mut cha = ValuePowers::new(one, cha);

                            let poly = AssignedUnivariatePoly::assign(
                                &mut region,
                                main_gate_config.clone(),
                                "test poly",
                                &poly,
                            )
                            .unwrap();

                            let main_gate = MainGate::new(main_gate_config.clone());

                            Ok(poly.eval(&mut region, &main_gate, &mut cha).unwrap())
                        },
                    )?;

                    assert_eq!(
                        off_circuit_res,
                        on_circuit_res.value().unwrap().copied().unwrap()
                    );

                    Ok(())
                }
            }

            MockProver::run(12, &TestCircuit {}, vec![])
                .unwrap()
                .verify()
                .unwrap();
        }

        #[traced_test]
        #[test]
        fn lagrange() {
            use crate::halo2curves::bn256::Fr;

            const L: usize = 3;

            struct TestCircuit;

            impl Circuit<Fr> for TestCircuit {
                type Config = MainGateConfig<T>;
                type FloorPlanner = SimpleFloorPlanner;

                fn without_witnesses(&self) -> Self {
                    todo!()
                }

                fn configure(meta: &mut ConstraintSystem<Fr>) -> Self::Config {
                    MainGate::configure(meta)
                }

                fn synthesize(
                    &self,
                    main_gate_config: Self::Config,
                    mut layouter: impl Layouter<Fr>,
                ) -> Result<(), Halo2PlonkError> {
                    let cha = Fr::from_u128(123);

                    dbg!(<Fr as PrimeField>::S);
                    let lagrange_domain = PolyContext::<Fr>::get_lagrange_domain::<L>();
                    debug!("lagrange_domain: {lagrange_domain}");

                    let [off_circuit_poly_L0_cha, off_circuit_poly_L1_cha] =
                        polynomial::iter_eval_lagrange_poly_for_cyclic_group::<Fr>(
                            cha,
                            lagrange_domain,
                        )
                        .take(2)
                        .collect::<Vec<_>>()
                        .try_into()
                        .unwrap();

                    let (on_circuit_poly_L0_cha, on_circuit_poly_L1_cha) = layouter.assign_region(
                        || "assigned_L0",
                        move |mut region| {
                            let main_gate = MainGate::<Fr, T>::new(main_gate_config.clone());
                            main_gate.config().name_columns(&mut region);

                            let mut region = RegionCtx::new(region, 0);

                            let cha = region
                                .assign_advice(
                                    || "",
                                    main_gate_config.state[0],
                                    Halo2Value::known(cha),
                                )
                                .unwrap();

                            let one = region
                                .assign_advice(
                                    || "",
                                    main_gate_config.state[1],
                                    Halo2Value::known(Fr::ONE),
                                )
                                .unwrap();

                            let mut values = ValuePowers::new(one, cha);

                            region.next();

                            Ok((
                                eval_lagrange_poly::<Fr, T, L>(
                                    &mut region,
                                    &main_gate,
                                    0,
                                    &mut values,
                                )?,
                                eval_lagrange_poly::<Fr, T, L>(
                                    &mut region,
                                    &main_gate,
                                    1,
                                    &mut values,
                                )?,
                            ))
                        },
                    )?;

                    assert_eq!(
                        off_circuit_poly_L0_cha,
                        on_circuit_poly_L0_cha.value().unwrap().copied().unwrap()
                    );

                    assert_eq!(
                        off_circuit_poly_L1_cha,
                        on_circuit_poly_L1_cha.value().unwrap().copied().unwrap()
                    );

                    Ok(())
                }
            }

            MockProver::run(12, &TestCircuit {}, vec![])
                .unwrap()
                .verify()
                .unwrap();
        }

        #[traced_test]
        #[test]
        fn vanishing() {
            const DEGREE: usize = 10;
            let cha = Base::from_u128(123);

            let off_circuit_vanishing = polynomial::lagrange::eval_vanish_polynomial(DEGREE, cha);

            let (mut wc, main_gate_config) = get_witness_collector();

            let mut layouter = SingleChipLayouter::new(&mut wc, vec![]).unwrap();

            let on_circuit_vanishing = layouter
                .assign_region(
                    || "vanishing",
                    move |region| {
                        let mut region = RegionCtx::new(region, 0);
                        let main_gate = MainGate::<Base, T>::new(main_gate_config.clone());

                        let cha = region
                            .assign_advice(|| "", main_gate_config.state[0], Halo2Value::known(cha))
                            .unwrap();

                        let one = region
                            .assign_advice(
                                || "",
                                main_gate_config.state[1],
                                Halo2Value::known(Base::ONE),
                            )
                            .unwrap();

                        region.next();

                        let mut cha = ValuePowers::new(one, cha);

                        eval_vanish_polynomial(&mut region, &main_gate, DEGREE, &mut cha)
                    },
                )
                .unwrap();

            assert_eq!(
                off_circuit_vanishing,
                on_circuit_vanishing.value().unwrap().copied().unwrap()
            );
        }

        #[traced_test]
        #[test]
        fn test_e() {
            use crate::halo2curves::bn256::Fr;

            struct TestCircuit;

            impl Circuit<Fr> for TestCircuit {
                type Config = MainGateConfig<T>;
                type FloorPlanner = SimpleFloorPlanner;

                fn without_witnesses(&self) -> Self {
                    todo!()
                }

                fn configure(meta: &mut ConstraintSystem<Fr>) -> Self::Config {
                    MainGate::configure(meta)
                }

                fn synthesize(
                    &self,
                    main_gate_config: Self::Config,
                    mut layouter: impl Layouter<Fr>,
                ) -> Result<(), Halo2PlonkError> {
                    const L: usize = 3;

                    let mut values = (0..).map(Into::into);
                    let proof = nifs::protogalaxy::Proof {
                        poly_F: UnivariatePoly::from_iter(values.by_ref().take(10)),
                        poly_K: UnivariatePoly::from_iter(values.by_ref().take(10)),
                    };

                    let gamma = values.next().unwrap();
                    let alpha = values.next().unwrap();

                    let log_n = PolyContext::<Fr>::get_lagrange_domain::<L>();

                    let off_circuit_e = nifs::protogalaxy::calculate_e(
                        &proof.poly_F,
                        &proof.poly_K,
                        gamma,
                        alpha,
                        log_n,
                    );

                    let on_circuit_e = layouter
                        .assign_region(
                            || "e",
                            move |region| {
                                let mut region = RegionCtx::new(region, 0);
                                let main_gate = MainGate::<Fr, T>::new(main_gate_config.clone());

                                let proof = AssignedProof::assign(
                                    &mut region,
                                    main_gate_config.clone(),
                                    proof.clone(),
                                )
                                .unwrap();

                                let one = region
                                    .assign_advice(
                                        || "",
                                        main_gate_config.state[0],
                                        Halo2Value::known(Fr::ONE),
                                    )
                                    .unwrap();
                                let gamma = region
                                    .assign_advice(
                                        || "",
                                        main_gate_config.state[1],
                                        Halo2Value::known(gamma),
                                    )
                                    .unwrap();

                                let alpha = region
                                    .assign_advice(
                                        || "",
                                        main_gate_config.state[2],
                                        Halo2Value::known(alpha),
                                    )
                                    .unwrap();

                                let mut gamma = ValuePowers::new(one.clone(), gamma);
                                let mut alpha = ValuePowers::new(one, alpha);

                                region.next();

                                calculate_e::<Fr, T, L>(
                                    &mut region,
                                    &main_gate,
                                    &proof,
                                    &mut gamma,
                                    &mut alpha,
                                )
                            },
                        )
                        .unwrap();

                    assert_eq!(
                        off_circuit_e,
                        on_circuit_e.value().unwrap().copied().unwrap()
                    );

                    Ok(())
                }
            }

            MockProver::run(12, &TestCircuit {}, vec![])
                .unwrap()
                .verify()
                .unwrap();
        }
    }
}
