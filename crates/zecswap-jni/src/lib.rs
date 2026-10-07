//! JNI bindings for `xyz.justzappit.atomicswap.AtomicSwapNative`, in the `android` module. Each
//! takes the wallet's 64-byte BIP-39 seed, the network and the swap's index, plus the 64-byte seed
//! of the Railgun wallet a note pays where it builds one, and computes what `ops` does; the token
//! bindings compute what `tokens` does. Errors and panics surface as `AtomicSwapException` instead
//! of crossing into the JVM.

pub mod ops;
pub mod tokens;

use std::panic::{AssertUnwindSafe, catch_unwind};

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{JNI_FALSE, jboolean, jbyteArray, jint, jlong, jobjectArray, jstring};
use zecswap_core::{Domain, NetworkType, SwapContext, SweepIntent};
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
    railgun_seed: JByteArray<'local>,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let railgun_seed = self::seed(env, &railgun_seed)?;
        let note = swap(&seed, mainnet, index)?.payout_note(&railgun_seed)?;
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
    railgun_seed: JByteArray<'local>,
    chain_id: jlong,
    contract: JByteArray<'local>,
    quote_id: JByteArray<'local>,
    maker_share: JByteArray<'local>,
    maker_proof: JByteArray<'local>,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let railgun_seed = self::seed(env, &railgun_seed)?;
        let context = SwapContext {
            chain_id: unsigned(chain_id, "a chain id")?,
            contract: fixed(env, &contract, "a contract address")?,
            quote_id: fixed(env, &quote_id, "a quote id")?,
        };
        let maker_share = fixed(env, &maker_share, "a maker share")?;
        let maker_proof = fixed(env, &maker_proof, "a maker proof")?;
        let accepted = swap(&seed, mainnet, index)?.accept(
            &railgun_seed,
            context,
            &maker_share,
            &maker_proof,
        )?;
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
    recipient: JString<'local>,
    minimum_received: jlong,
    maximum_fee: jlong,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let maker_share = fixed(env, &maker_share, "a maker share")?;
        let maker_secret = Zeroizing::new(fixed::<32>(env, &maker_secret, "a maker secret")?);
        let pczt = env.convert_byte_array(&pczt).map_err(jni_error)?;
        let recipient: String = env.get_string(&recipient).map_err(jni_error)?.into();
        let intent = SweepIntent::from_address(
            &recipient,
            if mainnet != JNI_FALSE {
                NetworkType::Main
            } else {
                NetworkType::Test
            },
            unsigned(minimum_received, "a minimum received amount")?,
            unsigned(maximum_fee, "a maximum sweep fee")?,
        )
        .map_err(|e| e.to_string())?;
        let signed = swap(&seed, mainnet, index)?.sign_refund(
            &maker_share,
            &maker_secret,
            &pczt,
            &intent,
        )?;
        byte_array(env, &signed)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_railgunAddress<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    railgun_seed: JByteArray<'local>,
) -> jstring {
    run(&mut env, |env| {
        let railgun_seed = self::seed(env, &railgun_seed)?;
        let address = ops::railgun_address(&railgun_seed)?;
        env.new_string(address)
            .map(JString::into_raw)
            .map_err(jni_error)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_signRefundPayout<'local>(
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
            swap(&seed, mainnet, index)?.sign_refund_payout(domain, &swap_id, &relayer, fee)?;
        byte_array(env, &signature)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_signRefundRescue<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
    railgun_seed: JByteArray<'local>,
    chain_id: jlong,
    contract: JByteArray<'local>,
    swap_id: JByteArray<'local>,
    relayer: JByteArray<'local>,
    fee: JString<'local>,
    nonce: jlong,
    deadline: jlong,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let railgun_seed = self::seed(env, &railgun_seed)?;
        let domain = domain(env, chain_id, &contract)?;
        let swap_id = fixed(env, &swap_id, "a swap id")?;
        let relayer = fixed(env, &relayer, "a relayer address")?;
        let fee: String = env.get_string(&fee).map_err(jni_error)?.into();
        let fee = fee
            .parse::<u128>()
            .map_err(|_| format!("a relayer fee can't be {fee}"))?;
        let signature = swap(&seed, mainnet, index)?.sign_refund_rescue(
            &railgun_seed,
            domain,
            &swap_id,
            &relayer,
            fee,
            zecswap_core::RescueAuthorization {
                nonce: unsigned(nonce, "a rescue nonce")?,
                deadline: unsigned(deadline, "a rescue deadline")?,
            },
        )?;
        byte_array(env, &signature)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_signReverseAction<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
    chain_id: jlong,
    contract: JByteArray<'local>,
    swap_id: JByteArray<'local>,
    deadline: jlong,
    action: jint,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let domain = domain(env, chain_id, &contract)?;
        let swap_id = fixed(env, &swap_id, "a swap id")?;
        let action = match action {
            0 => ops::ReverseAction::Ready,
            1 => ops::ReverseAction::LockRefund,
            _ => return Err("unknown reverse authorization".into()),
        };
        let signature = swap(&seed, mainnet, index)?.sign_reverse_action(
            domain,
            &swap_id,
            unsigned(deadline, "a deadline")?,
            action,
        )?;
        byte_array(env, &signature)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_signReverseOpen<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    seed: JByteArray<'local>,
    mainnet: jboolean,
    index: jint,
    railgun_seed: JByteArray<'local>,
    chain_id: jlong,
    contract: JByteArray<'local>,
    maker: JByteArray<'local>,
    token: JByteArray<'local>,
    amount: JString<'local>,
    maker_share: JByteArray<'local>,
    ready_deadline: jlong,
    refund_after: jlong,
    funding_deadline: jlong,
) -> jbyteArray {
    run(&mut env, |env| {
        let seed = self::seed(env, &seed)?;
        let railgun_seed = self::seed(env, &railgun_seed)?;
        let domain = domain(env, chain_id, &contract)?;
        let amount: String = env.get_string(&amount).map_err(jni_error)?.into();
        let terms = ops::ReverseTerms {
            maker: fixed(env, &maker, "a maker address")?,
            token: fixed(env, &token, "a token address")?,
            amount: amount
                .parse()
                .map_err(|_| "amount must fit uint128".to_owned())?,
            maker_share: fixed(env, &maker_share, "a maker share")?,
            ready_deadline: unsigned(ready_deadline, "ready deadline")?,
            refund_after: unsigned(refund_after, "refund time")?,
            funding_deadline: unsigned(funding_deadline, "funding deadline")?,
        };
        let signature =
            swap(&seed, mainnet, index)?.sign_reverse_open(&railgun_seed, domain, &terms)?;
        byte_array(env, &signature)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_readTokenChallenge<
    'local,
>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    header: JString<'local>,
) -> jobjectArray {
    run(&mut env, |env| {
        let header: String = env.get_string(&header).map_err(jni_error)?.into();
        let parts = tokens::read_challenge(&header)?;
        let array = env
            .new_object_array(3, "[B", JObject::null())
            .map_err(jni_error)?;
        for (i, part) in (0..).zip(parts) {
            let bytes = env.byte_array_from_slice(&part).map_err(jni_error)?;
            env.set_object_array_element(&array, i, bytes)
                .map_err(jni_error)?;
        }
        Ok(array.into_raw())
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_blindToken<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    token_key: JByteArray<'local>,
    challenge: JByteArray<'local>,
) -> jbyteArray {
    run(&mut env, |env| {
        let token_key = env.convert_byte_array(&token_key).map_err(jni_error)?;
        let challenge = env.convert_byte_array(&challenge).map_err(jni_error)?;
        let request = tokens::blind(&token_key, &challenge)?;
        byte_array(env, &request)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_finalizeToken<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    pending: JByteArray<'local>,
    token_key: JByteArray<'local>,
    blind_signature: JByteArray<'local>,
) -> jbyteArray {
    run(&mut env, |env| {
        let pending = Zeroizing::new(env.convert_byte_array(&pending).map_err(jni_error)?);
        let token_key = env.convert_byte_array(&token_key).map_err(jni_error)?;
        let blind_signature = env
            .convert_byte_array(&blind_signature)
            .map_err(jni_error)?;
        let token = tokens::finalize(&pending, &token_key, &blind_signature)?;
        byte_array(env, &token)
    })
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_xyz_justzappit_atomicswap_AtomicSwapNative_tokenAuthorization<
    'local,
>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    token: JByteArray<'local>,
) -> jstring {
    run(&mut env, |env| {
        let token = env.convert_byte_array(&token).map_err(jni_error)?;
        let authorization = tokens::authorization(&token)?;
        env.new_string(authorization)
            .map(JString::into_raw)
            .map_err(jni_error)
    })
    .unwrap_or(std::ptr::null_mut())
}
