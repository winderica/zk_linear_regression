use std::{
    borrow::Borrow,
    cmp::Ordering,
    fmt::{Debug, Display},
};

use crate::utils::{signed_to_field, ToBigUint};
use ark_ff::{One, PrimeField, Zero};
use ark_r1cs_std::{
    alloc::{AllocVar, AllocationMode},
    boolean::Boolean,
    fields::{fp::FpVar, FieldVar},
    prelude::EqGadget,
    R1CSVar, ToBitsGadget,
};
use ark_relations::r1cs::{Namespace, SynthesisError};
use num::{BigUint, Float, ToPrimitive};

#[derive(Clone, Debug)]
pub struct FloatVar<F: PrimeField> {
    pub sign: FpVar<F>,
    pub exponent: FpVar<F>,
    pub mantissa: FpVar<F>,
}

impl<F: PrimeField> Display for FloatVar<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Sign: {}\nExponent: {}\nMantissa: {}\n",
            &self.sign.value().unwrap_or(F::zero()),
            &self.exponent.value().unwrap_or(F::zero()),
            &self.mantissa.value().unwrap_or(F::zero())
        )
    }
}

impl<F: PrimeField> FloatVar<F> {
    pub fn verifier_input(i: f64) -> [F; 3] {
        let (mantissa, exponent, sign) = Float::integer_decode(i);
        let sign = match sign {
            1 => F::one(),
            -1 => -F::one(),
            _ => unreachable!(),
        };
        let mantissa = F::from(mantissa);
        let exponent = signed_to_field::<F, _>(exponent + 52);
        [sign, exponent, mantissa]
    }
}

impl<F: PrimeField> AllocVar<f64, F> for FloatVar<F> {
    fn new_variable<T: Borrow<f64>>(
        cs: impl Into<Namespace<F>>,
        f: impl FnOnce() -> Result<T, SynthesisError>,
        mode: AllocationMode,
    ) -> Result<Self, SynthesisError> {
        let i = *f()?.borrow();
        let cs = cs.into().cs();
        let (mantissa, exponent, sign) = Float::integer_decode(i);
        let sign = FpVar::new_variable(
            cs.clone(),
            || match sign {
                1 => Ok(F::one()),
                -1 => Ok(-F::one()),
                _ => Err(SynthesisError::AssignmentMissing),
            },
            mode,
        )?;
        let exponent = FpVar::new_variable(
            cs.clone(),
            || Ok(signed_to_field::<F, _>(exponent + 52)),
            mode,
        )?;
        let mantissa = FpVar::new_variable(cs.clone(), || Ok(F::from(mantissa)), mode)?;
        Ok(Self {
            sign,
            exponent,
            mantissa,
        })
    }
}

impl<F: PrimeField> ToBigUint for FpVar<F> {
    fn to_biguint(&self) -> BigUint {
        match self.value() {
            Ok(v) => v.into_repr().into(),
            Err(_) => BigUint::zero(),
        }
    }
}

impl<F: PrimeField> FloatVar<F> {
    pub fn equal(x: &Self, y: &Self) -> Result<(), SynthesisError> {
        x.sign.enforce_equal(&y.sign)?;
        x.exponent.enforce_equal(&y.exponent)?;
        x.mantissa.enforce_equal(&y.mantissa)?;
        Ok(())
    }

    pub fn neg(self) -> Self {
        Self {
            sign: FpVar::zero() - self.sign,
            exponent: self.exponent,
            mantissa: self.mantissa,
        }
    }

    pub fn add(cs: impl Into<Namespace<F>>, x: &Self, y: &Self) -> Result<Self, SynthesisError> {
        let cs = cs.into().cs();

        let two = FpVar::one().double()?;

        let b = x
            .exponent
            .is_cmp_unchecked(&y.exponent, Ordering::Less, false)?;

        let exponent = b.select(&y.exponent, &x.exponent)?;
        let delta = &exponent + &exponent - &x.exponent - &y.exponent;

        let max = FpVar::new_constant(cs.clone(), F::from(64u64))?;

        let delta = delta
            .is_cmp_unchecked(&max, Ordering::Greater, false)?
            .select(&max, &delta)?;

        let v = two.pow_le(&delta.to_bits_le()?)?;

        let xx = &x.sign * &x.mantissa;
        let yy = &y.sign * &y.mantissa;

        let unchanged = b.select(&xx, &yy)?;
        let changed = (&xx + &yy - &unchanged) * &v;

        let (sign, exponent, mantissa) = {
            let sum = changed + unchanged;

            let sign = sum
                .is_cmp_unchecked(&FpVar::zero(), Ordering::Less, false)?
                .select(&FpVar::one().negate()?, &FpVar::one())?;
            let sum = sum * &sign;

            let (q, e, r) = {
                let sum = sum.to_biguint();
                let delta = delta.to_biguint().to_i64().unwrap();

                let mut normalized = sum.clone();

                let mut delta_e = 0;
                if !normalized.is_zero() {
                    while normalized >= BigUint::one() << (delta + 53) {
                        delta_e += 1;
                        normalized >>= 1u8;
                    }
                    while normalized < BigUint::one() << (delta + 52) {
                        delta_e -= 1;
                        normalized <<= 1u8;
                    }
                    normalized >>= delta;
                } else {
                    delta_e = match exponent.negate()?.to_biguint().to_i64() {
                        Some(e) => e,
                        None => -exponent.to_biguint().to_i64().unwrap(),
                    } - 1023;
                }
                let r = if (delta + delta_e) <= 0 {
                    BigUint::zero()
                } else {
                    &sum - (&normalized << (delta + delta_e))
                };
                (
                    FpVar::new_witness(cs.clone(), || match F::BigInt::try_from(normalized) {
                        Ok(q) => Ok(F::from_repr(q).unwrap()),
                        Err(_) => panic!(),
                    })?,
                    FpVar::new_witness(cs.clone(), || Ok(signed_to_field::<F, _>(delta_e)))?,
                    FpVar::new_witness(cs.clone(), || match F::BigInt::try_from(r) {
                        Ok(r) => Ok(F::from_repr(r).unwrap()),
                        Err(_) => panic!(),
                    })?,
                )
            };

            q.is_zero()?
                .or(&q
                    .is_cmp(
                        &FpVar::new_constant(cs.clone(), F::from(1u64 << 52))?,
                        Ordering::Greater,
                        true,
                    )?
                    .and(&q.is_cmp(
                        &FpVar::new_constant(cs.clone(), F::from(1u64 << 53))?,
                        Ordering::Less,
                        false,
                    )?)?)?
                .enforce_equal(&Boolean::TRUE)?;

            let delta = &delta + &e;
            let b = delta.is_cmp_unchecked(&FpVar::zero(), Ordering::Greater, false)?;
            let m = b.select(&delta, &FpVar::zero())?;
            let n = &m - &delta;
            (&sum * two.pow_le(&n.to_bits_le()?)?)
                .enforce_equal(&(&q * two.pow_le(&m.to_bits_le()?)? + &r))?;
            // TODO: constraint on r?

            let u = b.select(
                &two.pow_le(&(&delta - FpVar::one()).to_bits_le()?)?,
                &FpVar::one(),
            )?;

            let q = &q
                + r.is_eq(&u)?.select(&q, &(u - r).double()?)?.to_bits_le()?[0]
                    .select(&FpVar::one(), &FpVar::zero())?;

            (sign, exponent + e, q)
        };

        Ok(FloatVar {
            sign,
            exponent,
            mantissa,
        })
    }

    pub fn mul(cs: impl Into<Namespace<F>>, x: &Self, y: &Self) -> Result<Self, SynthesisError> {
        let cs = cs.into().cs();

        let v = FpVar::new_constant(cs.clone(), F::from(1u64 << 52))?;
        let w = v.double()?;

        let sign = &x.sign * &y.sign;
        let (exponent, mantissa) = {
            let p = &x.mantissa * &y.mantissa;
            let b = &p.to_bits_le()?[105];

            let p = b.select(&p, &p.double()?)?;
            let e = &x.exponent + &y.exponent + b.select(&FpVar::one(), &FpVar::zero())?;

            let q = {
                let q = p.to_biguint() >> 53u8;

                FpVar::new_witness(cs.clone(), || match F::BigInt::try_from(q) {
                    Ok(q) => Ok(F::from_repr(q).unwrap()),
                    Err(_) => panic!(),
                })?
            };

            let r = p - &q * &w;
            r.enforce_cmp(&w, Ordering::Less, false)?;

            let q = &q
                + r.is_eq(&v)?.select(&q, &(v - r).double()?)?.to_bits_le()?[0]
                    .select(&FpVar::one(), &FpVar::zero())?;

            (e, q)
        };

        Ok(FloatVar {
            sign,
            exponent,
            mantissa,
        })
    }
}

#[cfg(test)]
mod tests {
    use ark_bls12_381::Bls12_381;
    use ark_groth16::{
        create_random_proof, generate_random_parameters, prepare_verifying_key, verify_proof,
    };
    use ark_relations::r1cs::{ConstraintSynthesizer, ConstraintSystemRef};
    use rand::{thread_rng, Rng};

    use super::*;

    #[test]
    fn test_add() {
        pub struct Circuit {
            a: f64,
            b: f64,
            c: f64,
        }

        impl<F: PrimeField> ConstraintSynthesizer<F> for Circuit {
            fn generate_constraints(
                self,
                cs: ConstraintSystemRef<F>,
            ) -> ark_relations::r1cs::Result<()> {
                let a = FloatVar::new_witness(cs.clone(), || Ok(self.a))?;
                let b = FloatVar::new_witness(cs.clone(), || Ok(self.b))?;
                let c = FloatVar::new_input(cs.clone(), || Ok(self.c))?;
                let d = FloatVar::add(cs, &a, &b)?;

                FloatVar::equal(&d, &c)?;
                Ok(())
            }
        }

        let rng = &mut thread_rng();

        let params = generate_random_parameters::<Bls12_381, _, _>(
            Circuit {
                a: 0f64,
                b: 0f64,
                c: 0f64,
            },
            rng,
        )
        .unwrap();
        let pvk = prepare_verifying_key(&params.vk);

        for _ in 0..100 {
            let a = -rng.gen::<f64>() * rng.gen::<u32>() as f64;
            let b = rng.gen::<f64>() * rng.gen::<u32>() as f64;

            println!("{} {}", a, b);
            let c = a + b;

            let proof = create_random_proof(Circuit { a, b, c }, &params, rng).unwrap();

            assert!(verify_proof(&pvk, &proof, &FloatVar::verifier_input(c)).unwrap());
        }
    }

    #[test]
    fn test_mul() {
        pub struct Circuit {
            a: f64,
            b: f64,
            c: f64,
        }

        impl<F: PrimeField> ConstraintSynthesizer<F> for Circuit {
            fn generate_constraints(
                self,
                cs: ConstraintSystemRef<F>,
            ) -> ark_relations::r1cs::Result<()> {
                let a = FloatVar::new_witness(cs.clone(), || Ok(self.a))?;
                let b = FloatVar::new_witness(cs.clone(), || Ok(self.b))?;
                let c = FloatVar::new_input(cs.clone(), || Ok(self.c))?;
                let d = FloatVar::mul(cs, &a, &b)?;

                FloatVar::equal(&d, &c)?;
                Ok(())
            }
        }

        let rng = &mut thread_rng();

        let params = generate_random_parameters::<Bls12_381, _, _>(
            Circuit {
                a: 0f64,
                b: 0f64,
                c: 0f64,
            },
            rng,
        )
        .unwrap();
        let pvk = prepare_verifying_key(&params.vk);

        for _ in 0..100 {
            let a = -rng.gen::<f64>();
            let b = rng.gen::<f64>() * 123456789000.;

            println!("{} {}", a, b);
            let c = a * b;

            let proof = create_random_proof(Circuit { a, b, c }, &params, rng).unwrap();

            assert!(verify_proof(&pvk, &proof, &FloatVar::verifier_input(c)).unwrap());
        }
    }
}
