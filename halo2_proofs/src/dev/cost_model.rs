//! Circuit cost model.
use std::{collections::BTreeMap, fs, io, iter, mem, time::Instant};

use crate::{
    arithmetic::{eval_polynomial, kate_division, parallelize, CurveAffine, Engine, Field},
    circuit::{Cell, Layouter, SimpleFloorPlanner},
    multicore,
    plonk::*,
    poly::{commitment::Params, commitment::ParamsVerifier, EvaluationDomain, Rotation},
    transcript::{Blake2bRead, Blake2bWrite, Challenge255},
};
use group::{prime::PrimeCurveAffine, GroupEncoding};
use pairing::bn256::{Bn256, Fr as Fp, G1Affine};
use rand_core::OsRng;
use rayon::current_num_threads;

use super::CircuitCost;

/// Measures the elapsed time of a closure.
pub fn measure_elapsed_time<T, F: FnOnce() -> T>(f: F) -> (f64, T) {
    let start = Instant::now();
    let res = f();
    (start.elapsed().as_secs_f64(), res)
}

/// EstimateResult is to store the output of estimate()
#[derive(Debug)]
pub struct EstimateResult {
    prover_time: f64,
    mem_usage: f64,
}

impl EstimateResult {
    /// print estimation result.
    pub fn print(&self) {
        println!("prover time = {} (s)", self.prover_time);
        println!("memory usage = {} (KB)", self.mem_usage);
    }
}

impl Calculation {
    // The returned argument is the number of additions and multiplications.
    fn fake_evaluate<F: Field>(&self) -> (usize, usize) {
        match self {
            Calculation::Add(_, _) => (1, 0),
            Calculation::Sub(_, _) => (1, 0),
            Calculation::Mul(_, _) => (0, 1),
            Calculation::Negate(_) => (1, 0),
            Calculation::LcBeta(_, _) => (1, 1),
            Calculation::LcTheta(_, _) => (1, 1),
            Calculation::AddGamma(_) => (1, 0),
            Calculation::Store(_) => (0, 0),
        }
    }
}

struct FakeProverQuery {
    rotation: Rotation,
}

impl<C: CurveAffine> Evaluator<C> {
    // Returns the number of hadamard addition and product operations.
    fn fake_evaluate_h(&self, pk: &ProvingKey<C>, l: usize) -> (usize, usize) {
        let cs = pk.get_vk().get_cs();
        let mut num_mul_lag = 0;
        let mut num_add_lag = 0;
        // All calculations, with cached intermediate results
        for calc in self.calculations.iter() {
            let (tmp_num_add_lag, tmp_num_mul_lag) = calc.calculation.fake_evaluate::<C::Scalar>();
            num_add_lag += tmp_num_add_lag;
            num_mul_lag += tmp_num_mul_lag;
        }

        // Accumulate value parts
        num_add_lag += self.value_parts.len();
        num_mul_lag += self.value_parts.len();

        for table_result in self.lookup_results.iter() {
            let (tmp_num_add_lag, tmp_num_mul_lag) = table_result.fake_evaluate::<C::Scalar>();
            num_add_lag += tmp_num_add_lag;
            num_mul_lag += tmp_num_mul_lag;
        }

        // Permutations
        let chunk_len = cs.degree() - 2;
        let num_perm_slices = (cs.permutation.get_columns().len() + chunk_len - 1) / chunk_len;

        if num_perm_slices > 0 {
            // Enforce only for the first set.
            // value(X) = value(X) * y + l_0(X) * (1 - z_0(X))
            num_add_lag += 2;
            num_mul_lag += 2;
            // Enforce only for the last set.
            // value(X) = value(X) * y + l_last(X) * (z_l(X)^2 - z_l(X))
            num_add_lag += 2;
            num_mul_lag += 3;
            // Except for the first set, enforce.
            // value(X) = value(X) * y + l_0(X) * (z_i(X) - z_{i-1}(\omega^(last) X))
            num_add_lag += 2 * (num_perm_slices - 1);
            num_mul_lag += 2 * (num_perm_slices - 1);
            // delta_start * beta_start
            num_mul_lag += 1;

            // And for all the sets we enforce:
            // (1 - (l_last(X) + l_blind(X))) * (
            //   z_i(\omega X) \prod_j (p(X) + \beta s_j(X) + \gamma)
            // - z_i(X) \prod_j (p(X) + \delta^j \beta X + \gamma)
            // )

            // Calculate left = z_i(\omega X) \prod_j (p(X) + \beta s_j(X) + \gamma)
            let mut tmp_num_add_lag = 0;
            let mut tmp_num_mul_lag = 0;
            tmp_num_add_lag += 2 * chunk_len;
            tmp_num_mul_lag += 2 * chunk_len;
            // Calculate right = z_i(X) \prod_j (p(X) + \delta^j \beta X + \gamma), current_delta *= DELTA
            tmp_num_add_lag += 2 * chunk_len;
            tmp_num_mul_lag += chunk_len;
            tmp_num_mul_lag += chunk_len;
            // value(X) = value(X) * y + (1 - (l_last(X) + l_blind(X))) * (
            //   z_i(\omega X) \prod_j (p(X) + \beta s_j(X) + \gamma)
            // - z_i(X) \prod_j (p(X) + \delta^j \beta X + \gamma)
            // ).
            tmp_num_add_lag += 2;
            tmp_num_mul_lag += 2;

            num_add_lag += tmp_num_add_lag * num_perm_slices;
            num_mul_lag += tmp_num_mul_lag * num_perm_slices;

            // beta_term *= &extended_omega;
            num_mul_lag += 1;
        }

        // Lookups
        let num_lookups = pk.get_vk().get_cs().lookups.len();
        // a_minus_s
        num_add_lag += num_lookups;
        // value(X) = value(X) * y + l_0(X) * (1 - z(X))
        num_add_lag += 2 * num_lookups;
        num_mul_lag += 2 * num_lookups;
        // value(X) = value(X) * y + l_last(X) * (z(X)^2 - z(X))
        num_add_lag += 2 * num_lookups;
        num_mul_lag += 3 * num_lookups;
        // value(X) = value(X) * y + (1 - (l_last(X) + l_blind(X))) * (
        //   z(\omega X) (a'(X) + \beta) (s'(X) + \gamma)
        //   - z(X) (\theta^{m-1} a_0(X) + ... + a_{m-1}(X) + \beta)
        //          (\theta^{m-1} s_0(X) + ... + s_{m-1}(X) + \gamma)
        // )
        num_add_lag += 4 * num_lookups;
        num_mul_lag += 5 * num_lookups;
        // value(X) = value(X) * y + l_0(X) * (a'(X) - s'(X))
        num_add_lag += 1 * num_lookups;
        num_mul_lag += 2 * num_lookups;
        // (1 - (l_last + l_blind)) * (a′(X) − s′(X))⋅(a′(X) − a′(\omega^{-1} X)) = 0
        num_add_lag += 2 * num_lookups;
        num_mul_lag += 3 * num_lookups;

        num_add_lag *= l;
        num_mul_lag *= l;

        (num_add_lag, num_mul_lag)
    }
}

/// estimate is to estimate the prover time, peek memory usage and aggregate circuit size.
pub fn estimate<E: Engine, ConcreteCircuit: Circuit<E::Scalar>>(
    circuit: ConcreteCircuit,
    k: usize,
) -> EstimateResult {
    // Generate small vk & pk
    let params: Params<E::G1Affine> = Params::<E::G1Affine>::unsafe_setup::<E>(15_u32);
    let vk = keygen_vk(&params, &circuit).expect("keygen_vk should not fail");
    let pk = keygen_pk(&params, vk, &circuit).expect("keygen_pk should not fail");

    let l = 1;

    // Initialize the polynomial commitment parameters
    let cs = pk.get_vk().get_cs();

    let generate_fake_params = |k| {
        let s = E::Scalar::random(OsRng);
        let rand_c1 = <E::G1Affine as PrimeCurveAffine>::generator() * s;
        let rand_c2 = <E::G2Affine as PrimeCurveAffine>::generator() * s;
        let rand_c1: E::G1Affine = rand_c1.into();
        let n = 1 << k;
        Params {
            k: k as u32,
            n: n as u64,
            g: (0..n).map(|_| rand_c1).collect(),
            g_lagrange: (0..n).map(|_| rand_c1).collect(),
            additional_data: Vec::from(rand_c2.to_bytes().as_ref()),
        }
    };

    let params = generate_fake_params(k);

    // Initialize the domain
    let domain = EvaluationDomain::new(cs.degree() as u32, params.k);

    let n = 1 << k as usize;
    let rand_ele = E::Scalar::random(&mut OsRng);
    let rand_vec: Vec<E::Scalar> = (0..n).map(|_| rand_ele.clone()).collect();
    let rand_vec2 = rand_vec.clone();
    let rand_values = domain.lagrange_from_vec(rand_vec.clone());

    // Estimate the time of each operation.
    //      msm
    let (time_msm, _) = measure_elapsed_time(|| params.commit_lagrange(&rand_values));
    //      fft
    let (time_fft, rand_poly) = measure_elapsed_time(|| domain.lagrange_to_coeff(rand_values));
    //      extended fft
    let (time_extended_fft, rand_coset) =
        measure_elapsed_time(|| domain.coeff_to_extended(rand_poly));
    //      BTree time cost in lookup argument
    let (time_btree, _) = measure_elapsed_time(|| {
        let mut leftover_table_map: BTreeMap<E::Scalar, u32> =
            rand_vec2
                .iter()
                .take(n)
                .fold(BTreeMap::new(), |mut acc, coeff| {
                    *acc.entry(*coeff).or_insert(0) += 1;
                    acc
                });
        for item in &rand_vec2 {
            if let Some(count) = leftover_table_map.get_mut(item) {
                *count -= 1;
            }
        }
    });

    // Estimate the number of each operation.
    let FuncCount {
        num_fft,
        num_extended_fft,
        num_msm,
        num_btree,
        num_add,
        num_mul,
        num_kate_div,
        num_add_lag,
        num_mul_lag,
        mem_usage,
    } = dummy_proof(&params, &pk, &domain, l);

    println!("num_fft = {}, time_fft = {}", num_fft, time_fft);
    println!(
        "num_extended_fft = {}, time_extended_fft = {}",
        num_extended_fft, time_extended_fft
    );
    println!("num_msm = {}, time_msm = {}", num_msm, time_msm);
    println!("num_btree = {}, time_btree = {}", num_btree, time_btree);

    let pt_non_linear = (num_fft as f64) * time_fft
        + (num_extended_fft as f64) * time_extended_fft
        + (num_msm as f64) * time_msm
        + (num_btree as f64) * time_btree;
    println!("pt_non_linear = {}\n", pt_non_linear);

    let mut rand_ext_vec: Vec<E::Scalar> = (0..domain.extended_len())
        .map(|_| rand_ele.clone())
        .collect();
    let (time_add_lag, _) = measure_elapsed_time(|| {
        parallelize(&mut rand_ext_vec, |rand_ext_vec, _| {
            for value in rand_ext_vec.iter_mut() {
                *value + rand_ele;
            }
        })
    });
    println!(
        "num_add_lag = {}, time_add_lag = {}",
        num_add_lag, time_add_lag
    );

    let (time_mul_lag, _) = measure_elapsed_time(|| {
        parallelize(&mut rand_ext_vec, |rand_ext_vec, _| {
            for value in rand_ext_vec.iter_mut() {
                *value * rand_ele;
            }
        })
    });
    println!(
        "num_mul_lag = {}, time_mul_lag = {}",
        num_mul_lag, time_mul_lag
    );

    let mut rand_vec: Vec<E::Scalar> = (0..n).map(|_| rand_ele.clone()).collect();
    let (time_add, _) = measure_elapsed_time(|| {
        parallelize(&mut rand_vec, |rand_vec, _| {
            for value in rand_vec.iter_mut() {
                *value + rand_ele;
            }
        })
    });
    println!("num_add = {}, time_add = {}", num_add, time_add);

    let (time_mul, _) = measure_elapsed_time(|| {
        parallelize(&mut rand_vec, |rand_vec, _| {
            for value in rand_vec.iter_mut() {
                *value * rand_ele;
            }
        })
    });
    println!("num_mul = {}, time_mul = {}", num_mul, time_mul);

    let (time_kate_div, _) = measure_elapsed_time(|| kate_division(&rand_vec, rand_ele));
    println!(
        "num_kate_div = {}, time_kate_div = {}",
        num_kate_div, time_kate_div
    );

    let pt_linear = (num_add_lag as f64) * time_add_lag
        + (num_mul_lag as f64) * time_mul_lag
        + (num_add as f64) * time_add
        + (num_mul as f64) * time_mul
        + (num_kate_div as f64) * time_kate_div;
    println!("pt_linear = {}", pt_linear);

    let (pt_random, _) = measure_elapsed_time(|| {
        let mut random_poly = domain.empty_coeff();
        for coeff in random_poly.iter_mut() {
            *coeff = E::Scalar::random(&mut OsRng);
        }
        random_poly
    });
    println!("pt_random = {}", pt_random);
    println!();

    let prover_time = pt_non_linear + pt_linear + pt_random;

    // let calc_linear_term = |x_1: f64, y_1: f64, x_2: f64, y_2: f64, x_3 :f64| {
    //     y_1 + (y_2 - y_1) / (x_2 - x_1) * (x_3 - x_1)
    // };

    // let mem_usage2 = calc_linear_term(
    //     (1 << res_1.k) as f64, res_1.mem_usage,
    //     (1 << res_2.k) as f64, res_2.mem_usage,
    //     (1 << k) as f64,
    // );
    // println!("mem_usage by linear regression = {}", mem_usage2);

    // calculate aggregate_circuit_size

    EstimateResult {
        prover_time,
        mem_usage: (mem_usage as f64) / 1024.0, // to KB
    }
}

/// Run a circuit proving process.
pub fn simulate_circuit<E: Engine, ConcreteCircuit: Circuit<E::Scalar>>(
    circuit: ConcreteCircuit,
    k: usize,
) {
    // let public_inputs_size = 0;

    // Initialize the polynomial commitment parameters
    let params: Params<E::G1Affine> = Params::<E::G1Affine>::unsafe_setup::<E>(k as u32);

    // Initialize the proving key
    let vk = keygen_vk(&params, &circuit).expect("keygen_vk should not fail");
    let pk = keygen_pk(&params, vk, &circuit).expect("keygen_pk should not fail");

    // Create a proof
    let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);

    let (prover_time, _) = measure_elapsed_time(|| {
        create_proof(&params, &pk, &[circuit], &[&[]], OsRng, &mut transcript)
            .expect("proof generation should not fail")
    });

    println!("k = {}, prover_time = {}", k, prover_time);
}

struct FuncCount {
    num_fft: usize,
    num_extended_fft: usize,
    num_msm: usize,
    num_btree: usize,
    num_add: usize,
    num_mul: usize,
    num_kate_div: usize,
    num_add_lag: usize,
    num_mul_lag: usize,
    mem_usage: usize,
}

fn dummy_proof<C: CurveAffine>(
    params: &Params<C>,
    pk: &ProvingKey<C>,
    domain: &EvaluationDomain<C::Scalar>,
    l: usize, // The number of input.
) -> FuncCount {
    let mut num_fft = 0_usize;
    let mut num_extended_fft = 0_usize;
    let mut num_msm = 0_usize;
    let mut num_btree = 0_usize;
    let mut num_add = 0_usize;
    let mut num_mul = 0_usize;
    let mut num_kate_div = 0_usize;

    let cs = pk.get_vk().get_cs();

    // (instance, advice) calculate (poly, coset, commitment)

    // ins_commit, pt += l * n_ins * commit_lagrange_t
    num_msm += l * cs.num_instance_columns;
    // ins_poly, pt += l * n_ins + lagrange_to_coeff_t
    num_fft += l * cs.num_instance_columns;
    // ins_coset, pt += l * n_ins + coeff_to_extended_t
    num_extended_fft += l * cs.num_instance_columns;
    // adv_commit, pt += l * n_adv * commit_lagrange_t
    num_msm += l * cs.num_advice_columns;
    // adv_poly, pt += l * n_adv * lagrange_to_coeff_t
    num_fft += l * cs.num_advice_columns;
    // adv_coset, pt += l * n_adv * coeff_to_extended_t
    num_extended_fft += l * cs.num_advice_columns;

    // pt += l * n_lookup * commit_permuted
    //      BTree cost for A' and S'.
    let num_lookups = cs.lookups.len();
    num_btree += l * num_lookups;

    // Commit to permutations.
    // l * perm_commit_t
    //      commit_lagrange: z
    let num_perm_slices =
        (cs.permutation.get_columns().len() + (cs.degree() - 3)) / (cs.degree() - 2);
    num_msm += l * num_perm_slices;
    //      lagrange_to_coeff: z
    num_fft += l * num_perm_slices;
    //      coeff_to_extended: z
    num_extended_fft += l * num_perm_slices;

    // pt += lookup_commit_product
    //      commit_lagrange: z, a', s'
    num_msm += l * 3 * num_lookups;
    //      lagrange_to_coeff: z, a', s'
    num_fft += l * 3 * num_lookups;

    // Commit to the vanishing argument's random polynomial for blinding h(x_3)
    // vanishing_commit
    //      commit: random_poly
    num_msm += 1;

    // Evaluate the h(X) polynomial
    // evaluate_h 3 coeff_to_extended for each lookup argument
    num_extended_fft += l * 3 * num_lookups;

    // Construct the vanishing argument's h(X) commitments
    // pt += vanishing_construct
    //      extended_to_coeff: h_poly
    num_extended_fft += 1;
    //      commit: h_poly_i
    let num_h_pieces = ((domain.extended_len() as u64 + params.n - 1) / params.n) as usize;
    num_msm += num_h_pieces;

    // Evaluate h.
    let (num_add_lag, num_mul_lag) = pk.get_ev().fake_evaluate_h(pk, l);

    // Estimate multiopen(gwc).
    //      commit: h_x, h_x
    //      The evaluations in multiopen is too small.
    // Initialize the query sets.
    let cs = pk.get_vk().get_cs();
    let queries = (0..l)
        .flat_map(|_| {
            iter::empty()
                .chain(
                    cs.instance_queries
                        .iter()
                        .map(move |&(_, at)| FakeProverQuery { rotation: at }),
                )
                .chain(
                    cs.advice_queries
                        .iter()
                        .map(move |&(_, at)| FakeProverQuery { rotation: at }),
                )
                .chain((0..num_perm_slices).flat_map(|_| {
                    iter::empty()
                        // Open permutation product commitments at x and \omega x
                        .chain(Some(FakeProverQuery {
                            rotation: Rotation::cur(),
                        }))
                        .chain(Some(FakeProverQuery {
                            rotation: Rotation::next(),
                        }))
                }))
                // Open it at \omega^{last} x for all but the last set. This rotation is only
                // sensical for the first row, but we only use this rotation in a constraint
                // that is gated on l_0.
                .chain((0..num_perm_slices).rev().skip(1).flat_map(|_| {
                    Some(FakeProverQuery {
                        rotation: Rotation(-1),
                    })
                }))
                .chain((0..num_lookups).flat_map(|_| {
                    iter::empty()
                        // Open lookup product commitments at x
                        .chain(Some(FakeProverQuery {
                            rotation: Rotation::cur(),
                        }))
                        // Open lookup input commitments at x
                        .chain(Some(FakeProverQuery {
                            rotation: Rotation::cur(),
                        }))
                        // Open lookup table commitments at x
                        .chain(Some(FakeProverQuery {
                            rotation: Rotation::cur(),
                        }))
                        // Open lookup input commitments at x_inv
                        .chain(Some(FakeProverQuery {
                            rotation: Rotation::prev(),
                        }))
                        // Open lookup product commitments at x_next
                        .chain(Some(FakeProverQuery {
                            rotation: Rotation::next(),
                        }))
                }))
        })
        .chain(
            cs.fixed_queries
                .iter()
                .map(|&(_, at)| FakeProverQuery { rotation: at }),
        )
        .chain(
            (0..cs.permutation.get_columns().len()).map(|_| FakeProverQuery {
                rotation: Rotation::cur(),
            }),
        )
        // We query the h(X) polynomial at x
        .chain(
            iter::empty()
                .chain(Some(FakeProverQuery {
                    rotation: Rotation::cur(),
                }))
                .chain(Some(FakeProverQuery {
                    rotation: Rotation::cur(),
                })),
        );
    let mut point_query_map: BTreeMap<Rotation, usize> = BTreeMap::new();
    for query in queries {
        if let Some(queries) = point_query_map.get_mut(&query.rotation) {
            *queries = *queries + 1_usize;
        } else {
            point_query_map.insert(query.rotation, 1);
        }
    }

    for rot in point_query_map.keys() {
        let cnt = point_query_map.get(rot).unwrap();
        // poly_batch = poly_batch * *v + poly;
        // eval_batch = eval_batch * *v + eval;
        num_add += cnt;
        num_mul += cnt;

        // poly_batch = &poly_batch - eval_batch;
        num_add += 1;
        num_kate_div += 1;
        num_msm += 1;
    }

    // Memory
    let mut mem_usage = 0_usize;
    //      instance / advice / fixed as value poly, and coset:
    let n = 1 << params.k as usize;
    let ext_n = domain.extended_len();
    mem_usage += l * (cs.num_instance_columns + cs.num_advice_columns) * (ext_n + 2 * n);
    mem_usage += cs.num_fixed_columns * (2 * n + ext_n);
    //      l_0, l_last, l_active_row as coset:
    mem_usage += 3 * ext_n;
    //      lookup compressed_input / compressed_table as value:
    // mem_usage += 2 * l * num_lookups * n;
    //      lookup permuted_input / permuted_table as value:
    // mem_usage += 2 * l * num_lookups * n;
    //      lookup permuted_input / permuted_table as poly:
    mem_usage += 2 * l * num_lookups * n;
    //      lookup Z as poly
    mem_usage += l * num_lookups * n;
    //      permutation sigma as value, poly, and coset:
    mem_usage += l * num_perm_slices * (2 * n + ext_n);
    //      permutation Z as poly,, and coset
    mem_usage += l * num_perm_slices * (n + ext_n);
    //      vanishing random_poly
    mem_usage += n;
    //      evaluate_h lookup values
    mem_usage += num_lookups * ext_n;
    //      evaluate_h single lookup Z / permuted_input / permuted_table as coset
    mem_usage += l * 3 * ext_n;
    //      evaluate_h h_poly as coset
    mem_usage += ext_n;

    println!("number of field element: {}", mem_usage);

    mem_usage *= mem::size_of::<C::Scalar>();

    FuncCount {
        num_fft,
        num_extended_fft,
        num_msm,
        num_btree,
        num_add,
        num_mul,
        num_kate_div,
        num_add_lag,
        num_mul_lag,
        mem_usage,
    }
}

/// Generate a main function to run the cost model for a circuit.
#[macro_export]
macro_rules! cost_model_main {
    ($cir:expr) => {
        use halo2_proofs::dev::{estimate, simulate_circuit};

        fn main() {
            let mode = std::env::args().nth(1).expect("no running-mode given");
            let k = std::env::args()
                .nth(2)
                .expect("no circuit size given")
                .parse()
                .unwrap();
            let circuit = $cir;
            if mode.eq(&String::from("simulate")) {
                simulate_circuit::<Bn256, _>(circuit, k);
            } else if mode.eq(&String::from("estimate")) {
                let res = estimate::<Bn256, _>(circuit, k);
                res.print();
            } else {
                panic!("unrecognized format");
            }
        }
    };
}