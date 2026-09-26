//! JNI bindings for `xyz.justzappit.atomicswap.AtomicSwapNative`, in the `android` module. Each
//! takes the wallet's 64-byte BIP-39 seed, the network and the swap's index, and computes what
//! `ops` does; errors and panics surface as `AtomicSwapException` instead of crossing into the JVM.

pub mod ops;

use std::panic::{AssertUnwindSafe, catch_unwind};

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{JNI_FALSE, jboolean, jbyteArray, jint, jlong, jobjectArray, jstring};
use zecswap_core::{Domain, SwapContext};
use zeroize::Zeroizing;

use crate::ops::{Result, Swap};

const EXCEPTION: &str = "xyz/justzappit/atomicswap/AtomicSwapException";

/// Runs `body`, leaving an `AtomicSwapException` pending if it fails or panics.
fn run<'local, T>(
    env: &mut JNIEnv<'local>,
    body: impl FnOnce(&mut JNIEnv<'local>) -> Result<T>,
) -> Option<T> {
    let message = match catch_unwind(AssertUnwindSafe(|| body(&mut *env))) {
        Ok(Ok(value)) => return Some(value),
        Ok(Err(message)) => message,
        Err(_) => "zecswap panicked".to_owned(),
    };
    // A failed JNI call leaves its own exception pending, which says more than ours would.
    if !env.exception_check().unwrap_or(true) {
        let _ = env.throw_new(EXCEPTION, message);
    }
    None
}

fn swap<'a>(seed: &'a [u8], mainnet: jboolean, index: jint) -> Result<Swap<'a>> {
    Swap::new(seed, mainnet != JNI_FALSE, index)
}

fn seed(env: &JNIEnv, array: &JByteArray) -> Result<Zeroizing<Vec<u8>>> {
    Ok(Zeroizing::new(
        env.convert_byte_array(array).map_err(jni_error)?,
    ))
}

fn fixed<const N: usize>(env: &JNIEnv, array: &JByteArray, what: &str) -> Result<[u8; N]> {
    let bytes = env.convert_byte_array(array).map_err(jni_error)?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("{what} must be {N} bytes, not {}", bytes.len()))
}

fn unsigned(value: jlong, what: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| format!("{what} can't be {value}"))
}

fn domain(env: &JNIEnv, chain_id: jlong, contract: &JByteArray) -> Result<Domain> {
    Ok(Domain {
        chain_id: unsigned(chain_id, "a chain id")?,
        contract: fixed(env, contract, "a contract address")?,
    })
}

fn byte_array(env: &mut JNIEnv, bytes: &[u8]) -> Result<jbyteArray> {
    env.byte_array_from_slice(bytes)
        .map(JByteArray::into_raw)
        .map_err(jni_error)
}

fn jni_error(e: jni::errors::Error) -> String {
    e.to_string()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_userShare<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let share = swap(&seed, mainnet, index)?.user_share()?;
        byte_array(env, &share)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_authAddress<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let address = swap(&seed, mainnet, index)?.auth_address()?;
        byte_array(env, &address)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_claimSecret<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let secret = Zeroizing::new(swap(&seed, mainnet, index)?.claim_secret()?);
        byte_array(env, &*secret)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_payoutNote<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let note = swap(&seed, mainnet, index)?.payout_note()?;
        byte_array(env, &note)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_accept<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
    chain_id: jlong,
    contract: JByteArray<'local>,
    quote_id: JByteArray<'local>,
    maker_share: JByteArray<'local>,
    maker_proof: JByteArray<'local>,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let context = SwapContext {
            chain_id: unsigned(chain_id, "a chain id")?,
            contract: fixed(env, &contract, "a contract address")?,
            quote_id: fixed(env, &quote_id, "a quote id")?,
        };
        let maker_share = fixed(env, &maker_share, "a maker share")?;
        let maker_proof = fixed(env, &maker_proof, "a maker proof")?;
        let accepted = swap(&seed, mainnet, index)?.accept(context, &maker_share, &maker_proof)?;
        byte_array(env, &accepted)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_depositAccount<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
    maker_share: JByteArray<'local>,
) -> jobjectArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let maker_share = fixed(env, &maker_share, "a maker share")?;
        let account = swap(&seed, mainnet, index)?.deposit_account(&maker_share)?;
        let array = env
            .new_object_array(2, "java/lang/String", JObject::null())
            .map_err(jni_error)?;
        for (i, value) in (0..).zip(account) {
            let string = env.new_string(value).map_err(jni_error)?;
            env.set_object_array_element(&array, i, string)
                .map_err(jni_error)?;
        }
        Ok(array.into_raw())
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_signLockClaim<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
    chain_id: jlong,
    contract: JByteArray<'local>,
    swap_id: JByteArray<'local>,
    deadline: jlong,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let domain = domain(env, chain_id, &contract)?;
        let swap_id = fixed(env, &swap_id, "a swap id")?;
        let deadline = unsigned(deadline, "a deadline")?;
        let signature = swap(&seed, mainnet, index)?.sign_lock_claim(domain, &swap_id, deadline)?;
        byte_array(env, &signature)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_signPayout<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
    chain_id: jlong,
    contract: JByteArray<'local>,
    swap_id: JByteArray<'local>,
    relayer: JByteArray<'local>,
    fee: JString<'local>,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let domain = domain(env, chain_id, &contract)?;
        let swap_id = fixed(env, &swap_id, "a swap id")?;
        let relayer = fixed(env, &relayer, "a relayer address")?;
        let fee: String = env.get_string(&fee).map_err(jni_error)?.into();
        let fee = fee
            .parse::<u128>()
            .map_err(|_| format!("a relayer fee can't be {fee}"))?;
        let signature =
            swap(&seed, mainnet, index)?.sign_payout(domain, &swap_id, &relayer, fee)?;
        byte_array(env, &signature)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_signRefund<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
    maker_share: JByteArray<'local>,
    maker_secret: JByteArray<'local>,
    pczt: JByteArray<'local>,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let maker_share = fixed(env, &maker_share, "a maker share")?;
        let maker_secret = Zeroizing::new(fixed::<32>(env, &maker_secret, "a maker secret")?);
        let pczt = env.convert_byte_array(&pczt).map_err(jni_error)?;
        let signed =
            swap(&seed, mainnet, index)?.sign_refund(&maker_share, &maker_secret, &pczt)?;
        byte_array(env, &signed)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_railgunAddress<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
) -> jstring {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let address = ops::railgun_address(&seed)?;
        env.new_string(address)
            .map(JString::into_raw)
            .map_err(jni_error)
    })
    .unwrap_or(std::ptr::null_mut())
}
