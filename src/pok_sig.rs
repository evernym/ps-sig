// Proof of knowledge of signature

use crate::errors::PSError;
use crate::keys::{Params, Verkey};
use crate::signature::Signature;
use crate::blind_signature::{BlindingKey, BlindSignature};
use crate::{ate_2_pairing, OtherGroup, OtherGroupVec, SignatureGroup, SignatureGroupVec};
use amcl_wrapper::field_elem::{FieldElement, FieldElementVector};
use amcl_wrapper::group_elem::{GroupElement, GroupElementVector};
use amcl_wrapper::group_elem_g1::{G1Vector, G1};
use amcl_wrapper::group_elem_g2::{G2Vector, G2};
use std::collections::{HashMap, HashSet};

// Implement proof of knowledge of committed values in a vector commitment for `SignatureGroup`

impl_PoK_VC!(
    ProverCommittingOtherGroup,
    ProverCommittedOtherGroup,
    ProofOtherGroup,
    OtherGroup,
    OtherGroupVec
);

/*
As section 6.2 describes, for proving knowledge of a signature, the signature sigma is first randomized and also
transformed into a sequential aggregate signature with extra message t for public key g_tilde (and secret key 1).
1. Say the signature sigma is transformed to sigma_prime = (sigma_prime_1, sigma_prime_2) like step 1 in 6.2
1. The prover then sends sigma_prime and the value J = X_tilde * Y_tilde_1^m1 * Y_tilde_2^m2 * ..... * g_tilde^t and the proof J is formed correctly.
The verifier now checks whether e(sigma_prime_1, J) == e(sigma_prime_2, g_tilde). Since X_tilde is known,
the verifier can send following a modified value J' where J' = Y_tilde_1^m_1 * Y_tilde_2^m_2 * ..... * g_tilde^t with the proof of knowledge of elements of J'.
The verifier will then check the pairing e(sigma_prime_1, J'*X_tilde) == e(sigma_prime_2, g_tilde).

To reveal some of the messages from the signature but not all, in above protocol, construct J to be of the hidden values only, the verifier will
then add the revealed values (raised to the respective generators) to get a final J which will then be used in the pairing check.
*/
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoKOfSignature {
    pub secrets: FieldElementVector,
    pub sig: Signature,
    pub J: OtherGroup,
    pub pok_vc: ProverCommittedOtherGroup,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoKOfSignatureProof {
    pub sig: Signature,
    pub J: OtherGroup,
    pub proof_vc: ProofOtherGroup,
}

impl PoKOfSignature {
    /// Section 6.2 of paper
    pub fn init(
        sig: &Signature,
        vk: &Verkey,
        params: &Params,
        messages: &[FieldElement],
        blindings: Option<&[FieldElement]>,
        revealed_msg_indices: HashSet<usize>,
    ) -> Result<Self, PSError> {
        for idx in &revealed_msg_indices {
            if *idx >= messages.len() {
                return Err(PSError::GeneralError {
                    msg: format!("Index {} should be less than {}", idx, messages.len()),
                });
            }
        }
        Signature::check_verkey_and_messages_compat(messages, vk)?;
        let mut blindings: Vec<Option<&FieldElement>> = match blindings {
            Some(b) => {
                if (messages.len() - revealed_msg_indices.len()) != b.len() {
                    return Err(PSError::GeneralError {
                        msg: format!(
                            "No of blindings {} not equal to number of hidden messages {}",
                            b.len(),
                            (messages.len() - revealed_msg_indices.len())
                        ),
                    });
                }
                b.iter().map(Some).collect()
            }
            None => (0..(messages.len() - revealed_msg_indices.len()))
                .map(|_| None)
                .collect(),
        };

        let r = FieldElement::random();
        let t = FieldElement::random();

        // Transform signature to an aggregate signature on (messages, t)
        let sigma_prime_1 = &sig.sigma_1 * &r;
        let sigma_prime_2 = (&sig.sigma_2 + (&sig.sigma_1 * &t)) * &r;

        // +1 for `t`
        let hidden_msg_count = vk.Y_tilde.len() - revealed_msg_indices.len() + 1;
        let mut bases = OtherGroupVec::with_capacity(hidden_msg_count);
        let mut exponents = FieldElementVector::with_capacity(hidden_msg_count);
        bases.push(params.g_tilde.clone());
        exponents.push(t.clone());
        for i in 0..vk.Y_tilde.len() {
            if revealed_msg_indices.contains(&i) {
                continue;
            }
            bases.push(vk.Y_tilde[i].clone());
            exponents.push(messages[i].clone());
        }
        // Prove knowledge of m_1, m_2, ... for all hidden m_i and t in J = Y_tilde_1^m_1 * Y_tilde_2^m_2 * ..... * g_tilde^t
        let J = bases.multi_scalar_mul_const_time(&exponents).unwrap();

        // For proving knowledge of messages in J.
        // Choose blinding for g_tilde randomly
        blindings.insert(0, None);
        let mut committing = ProverCommittingOtherGroup::new();
        for b in bases.as_slice() {
            committing.commit(b, blindings.remove(0));
        }
        let committed = committing.finish();

        let sigma_prime = Signature {
            sigma_1: sigma_prime_1,
            sigma_2: sigma_prime_2,
        };
        Ok(Self {
            secrets: exponents,
            sig: sigma_prime,
            J,
            pok_vc: committed,
        })
    }

    /// Return byte representation of public elements so they can be used for challenge computation
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![];
        bytes.append(&mut self.sig.to_bytes());
        bytes.append(&mut self.J.to_bytes());
        bytes.append(&mut self.pok_vc.to_bytes());
        bytes
    }

    pub fn gen_proof(self, challenge: &FieldElement) -> Result<PoKOfSignatureProof, PSError> {
        let proof_vc = self.pok_vc.gen_proof(challenge, self.secrets.as_slice())?;
        Ok(PoKOfSignatureProof {
            sig: self.sig,
            J: self.J,
            proof_vc,
        })
    }
}

impl PoKOfSignatureProof {
    pub fn verify(
        &self,
        vk: &Verkey,
        params: &Params,
        revealed_msgs: HashMap<usize, FieldElement>,
        challenge: &FieldElement,
    ) -> Result<bool, PSError> {
        if self.sig.sigma_1.is_identity() || self.sig.sigma_2.is_identity() {
            return Ok(false);
        }

        // +1 for `t`
        let hidden_msg_count = vk.Y_tilde.len() - revealed_msgs.len() + 1;
        let mut bases = OtherGroupVec::with_capacity(hidden_msg_count);
        bases.push(params.g_tilde.clone());
        for i in 0..vk.Y_tilde.len() {
            if revealed_msgs.contains_key(&i) {
                continue;
            }
            bases.push(vk.Y_tilde[i].clone());
        }
        if !self.proof_vc.verify(bases.as_slice(), &self.J, challenge)? {
            return Ok(false);
        }
        // e(sigma_prime_1, J*X_tilde) == e(sigma_prime_2, g_tilde) => e(sigma_prime_1, J*X_tilde) * e(sigma_prime_2^-1, g_tilde) == 1
        let mut j;
        let J = if revealed_msgs.is_empty() {
            &self.J
        } else {
            j = self.J.clone();
            let mut b = OtherGroupVec::with_capacity(revealed_msgs.len());
            let mut e = FieldElementVector::with_capacity(revealed_msgs.len());
            for (i, m) in revealed_msgs {
                b.push(vk.Y_tilde[i].clone());
                e.push(m.clone());
            }
            j += b.multi_scalar_mul_var_time(&e).unwrap();
            &j
        };
        // e(sigma_1, (J + &X_tilde)) == e(sigma_2, g_tilde) => e(sigma_1, (J + &X_tilde)) * e(-sigma_2, g_tilde) == 1
        // Slight optimization possible by precomputing inverse of g_tilde and storing to avoid inverse of sig.sigma_2
        let res = ate_2_pairing(
            &self.sig.sigma_1,
            &(J + &vk.X_tilde),
            &(-&self.sig.sigma_2),
            &params.g_tilde,
        );
        Ok(res.is_one())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // For benchmarking
    use crate::keys::keygen;
    use std::time::{Duration, Instant};

    impl_PoK_VC!(
        ProverCommittingSignatureGroup,
        ProverCommittedSignatureGroup,
        ProofSignatureGroup,
        SignatureGroup,
        SignatureGroupVec
    );

    #[test]
    fn test_PoK_VC_SignatureGroup() {
        let n = 5;

        test_PoK_VC!(
            n,
            ProverCommittingSignatureGroup,
            ProverCommittedSignatureGroup,
            ProofSignatureGroup,
            SignatureGroup,
            SignatureGroupVec
        );
    }

    #[test]
    fn test_PoK_VC_OtherGroup() {
        let n = 5;

        test_PoK_VC!(
            n,
            ProverCommittingOtherGroup,
            ProverCommittedOtherGroup,
            ProofOtherGroup,
            OtherGroup,
            OtherGroupVec
        );
    }

    #[test]
    fn test_PoK_sig() {
        let count_msgs = 5;
        let params = Params::new("test".as_bytes());
        let (sk, vk) = keygen(count_msgs, &params);

        let msgs = FieldElementVector::random(count_msgs);
        let sig = Signature::new(msgs.as_slice(), &sk, &params).unwrap();
        assert!(sig.verify(msgs.as_slice(), &vk, &params).unwrap());

        let pok = PoKOfSignature::init(&sig, &vk, &params, msgs.as_slice(), None, HashSet::new()).unwrap();

        let chal = pok.pok_vc.gen_challenge(pok.J.to_bytes());

        let proof = pok.gen_proof(&chal).unwrap();

        assert!(proof.verify(&vk, &params, HashMap::new(), &chal).unwrap());

        // PoK with supplied blindings
        let blindings = FieldElementVector::random(count_msgs);
        let pok_1 = PoKOfSignature::init(
            &sig,
            &vk,
            &params,
            msgs.as_slice(),
            Some(blindings.as_slice()),
            HashSet::new(),
        )
        .unwrap();
        let chal_1 = FieldElement::from_msg_hash(&pok_1.to_bytes());
        let proof_1 = pok_1.gen_proof(&chal_1).unwrap();

        assert!(proof_1.verify(&vk, &params, HashMap::new(), &chal_1).unwrap());
    }

    #[test]
    fn test_PoK_sig_reveal_messages() {
        let count_msgs = 10;
        let params = Params::new("test".as_bytes());
        let (sk, vk) = keygen(count_msgs, &params);

        let msgs = FieldElementVector::random(count_msgs);

        let sig = Signature::new(msgs.as_slice(), &sk, &params).unwrap();
        assert!(sig.verify(msgs.as_slice(), &vk, &params).unwrap());

        let mut revealed_msg_indices = HashSet::new();
        revealed_msg_indices.insert(2);
        revealed_msg_indices.insert(4);
        revealed_msg_indices.insert(9);

        let pok = PoKOfSignature::init(
            &sig,
            &vk,
            &params,
            msgs.as_slice(),
            None,
            revealed_msg_indices.clone(),
        )
        .unwrap();

        let chal = pok.pok_vc.gen_challenge(pok.J.to_bytes());

        let proof = pok.gen_proof(&chal).unwrap();

        let mut revealed_msgs = HashMap::new();
        for i in &revealed_msg_indices {
            revealed_msgs.insert(i.clone(), msgs[*i].clone());
        }
        assert!(proof.verify(&vk, &params, revealed_msgs.clone(), &chal).unwrap());

        // Reveal wrong message
        let mut revealed_msgs_1 = revealed_msgs.clone();
        revealed_msgs_1.insert(2, FieldElement::random());
        assert!(!proof.verify(&vk, &params, revealed_msgs_1.clone(), &chal).unwrap());
    }

    #[test]
    fn test_PoK_multiple_sigs() {
        // Prove knowledge of multiple signatures together (using the same challenge)
        let count_msgs = 5;
        let params = Params::new("test".as_bytes());
        let (sk, vk) = keygen(count_msgs, &params);

        let msgs_1 = FieldElementVector::random(count_msgs);
        let sig_1 = Signature::new(msgs_1.as_slice(), &sk, &params).unwrap();
        assert!(sig_1.verify(msgs_1.as_slice(), &vk, &params).unwrap());

        let msgs_2 = FieldElementVector::random(count_msgs);
        let sig_2 = Signature::new(msgs_2.as_slice(), &sk, &params).unwrap();
        assert!(sig_2.verify(msgs_2.as_slice(), &vk, &params).unwrap());

        let pok_1 =
            PoKOfSignature::init(&sig_1, &vk, &params, msgs_1.as_slice(), None, HashSet::new()).unwrap();
        let pok_2 =
            PoKOfSignature::init(&sig_2, &vk, &params, msgs_2.as_slice(), None, HashSet::new()).unwrap();

        let mut chal_bytes = vec![];
        chal_bytes.append(&mut pok_1.to_bytes());
        chal_bytes.append(&mut pok_2.to_bytes());

        let chal = FieldElement::from_msg_hash(&chal_bytes);

        let proof_1 = pok_1.gen_proof(&chal).unwrap();
        let proof_2 = pok_2.gen_proof(&chal).unwrap();

        assert!(proof_1.verify(&vk, &params, HashMap::new(), &chal).unwrap());
        assert!(proof_2.verify(&vk, &params, HashMap::new(), &chal).unwrap());
    }

    #[test]
    fn test_PoK_multiple_sigs_with_same_msg() {
        // Prove knowledge of multiple signatures and the equality of a specific message under both signatures.
        // Knowledge of 2 signatures and their corresponding messages is being proven.
        // 2nd message in the 1st signature and 5th message in the 2nd signature are to be proven equal without revealing them

        let count_msgs = 5;
        let params = Params::new("test".as_bytes());
        let (sk, vk) = keygen(count_msgs, &params);

        let same_msg = FieldElement::random();
        let mut msgs_1 = FieldElementVector::random(count_msgs - 1);
        msgs_1.insert(1, same_msg.clone());
        let sig_1 = Signature::new(msgs_1.as_slice(), &sk, &params).unwrap();
        assert!(sig_1.verify(msgs_1.as_slice(), &vk, &params).unwrap());

        let mut msgs_2 = FieldElementVector::random(count_msgs - 1);
        msgs_2.insert(4, same_msg.clone());
        let sig_2 = Signature::new(msgs_2.as_slice(), &sk, &params).unwrap();
        assert!(sig_2.verify(msgs_2.as_slice(), &vk, &params).unwrap());

        // A particular message is same
        assert_eq!(msgs_1[1], msgs_2[4]);

        let same_blinding = FieldElement::random();

        let mut blindings_1 = FieldElementVector::random(count_msgs - 1);
        blindings_1.insert(1, same_blinding.clone());

        let mut blindings_2 = FieldElementVector::random(count_msgs - 1);
        blindings_2.insert(4, same_blinding.clone());

        // Blinding for the same message is kept same
        assert_eq!(blindings_1[1], blindings_2[4]);

        let pok_1 = PoKOfSignature::init(
            &sig_1,
            &vk, &params,
            msgs_1.as_slice(),
            Some(blindings_1.as_slice()),
            HashSet::new(),
        )
        .unwrap();
        let pok_2 = PoKOfSignature::init(
            &sig_2,
            &vk, &params,
            msgs_2.as_slice(),
            Some(blindings_2.as_slice()),
            HashSet::new(),
        )
        .unwrap();

        let mut chal_bytes = vec![];
        chal_bytes.append(&mut pok_1.to_bytes());
        chal_bytes.append(&mut pok_2.to_bytes());

        let chal = FieldElement::from_msg_hash(&chal_bytes);

        let proof_1 = pok_1.gen_proof(&chal).unwrap();
        let proof_2 = pok_2.gen_proof(&chal).unwrap();

        // Response for the same message should be same (this check is made by the verifier)
        // 1 added to the index, since 0th index is reserved for randomization (`t`)
        // XXX: Does adding a `get_resp_for_message` to `proof` make sense to abstract this detail of +1.
        assert_eq!(
            proof_1.proof_vc.responses[1 + 1],
            proof_2.proof_vc.responses[1 + 4]
        );

        assert!(proof_1.verify(&vk, &params, HashMap::new(), &chal).unwrap());
        assert!(proof_2.verify(&vk, &params, HashMap::new(), &chal).unwrap());
    }

    #[test]
    fn timing_pok_signature() {
        // Measure time to prove knowledge of signatures, both generation and verification of proof
        let iterations = 100;
        let count_msgs = 10;
        let params = Params::new("test".as_bytes());
        let (sk, vk) = keygen(count_msgs, &params);

        let msgs = FieldElementVector::random(count_msgs);
        let sig = Signature::new(msgs.as_slice(), &sk, &params).unwrap();

        let mut total_generating = Duration::new(0, 0);
        let mut total_verifying = Duration::new(0, 0);

        for _ in 0..iterations {
            let start = Instant::now();

            let pok =
                PoKOfSignature::init(&sig, &vk, &params, msgs.as_slice(), None, HashSet::new()).unwrap();

            let chal = pok.pok_vc.gen_challenge(pok.J.to_bytes());

            let proof = pok.gen_proof(&chal).unwrap();
            total_generating += start.elapsed();

            let start = Instant::now();
            assert!(proof.verify(&vk, &params, HashMap::new(), &chal).unwrap());
            total_verifying += start.elapsed();
        }

        println!(
            "Time to create {} proofs is {:?}",
            iterations, total_generating
        );
        println!(
            "Time to verify {} proofs is {:?}",
            iterations, total_verifying
        );
    }
}
