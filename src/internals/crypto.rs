use blake3::Hash;
use ed25519_dalek_blake3::{Keypair, Signature, PublicKey, SecretKey, ExpandedSecretKey, Verifier, SignatureError};
use rand::Rng;
use rand::rngs::OsRng;


fn generate_keypair() -> Keypair {
    let mut cspring = OsRng {};
    Keypair::generate(&mut cspring)
}

fn sign_data(public_key: &PublicKey, secret_key: &SecretKey, message: &[u8]) -> Signature {
    let expanded_secret_key = ExpandedSecretKey::from(secret_key);
    expanded_secret_key.sign(message, public_key)
}

fn verify_data(public_key: &PublicKey, message: &[u8], signature: &Signature) -> Result<(), SignatureError> {
    public_key.verify(message, signature)
}

pub fn generate_random_token() -> Hash {
    let random = format!("{:?}", rand::thread_rng().gen::<f64>());
    blake3::hash(random.as_bytes())
}

fn generate_discovery_key(name: &[u8]) -> Hash {
    blake3::hash(name)
}

#[cfg(test)]
mod tests {
    use crate::internals::crypto::{generate_keypair, sign_data, verify_data};

    #[test]
    fn test_sign_unsign() {
        let keypair = generate_keypair();
        let message = "Wish you were here...";
        let signature = sign_data(&keypair.public, &keypair.secret, message.as_bytes());
        assert!(verify_data(&keypair.public, message.as_bytes(), &signature).is_ok(),"Can't verify signed data.");
        assert!(verify_data(&keypair.public, "Same old fears...".as_bytes(), &signature).is_err(),"Verified wrong message.")
    }
}

