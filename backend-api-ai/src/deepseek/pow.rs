//! DeepSeek proof-of-work ("DeepSeekHashV1").
//!
//! The algorithm is a custom hash construction, not plain SHA3-256 or Keccak-256
//! (verified by differential testing against the reference implementation), so
//! re-implementing it in Rust would be guesswork. Instead we embed the 26 KB
//! reference module and run it with `wasmi` — a pure-Rust WebAssembly
//! interpreter, no C toolchain and no Node.js at runtime.
//!
//! Exported surface of the module:
//!   memory
//!   wasm_solve(retptr, ch_ptr, ch_len, prefix_ptr, prefix_len, difficulty)
//!   __wbindgen_export_0(len, align) -> ptr      (allocator)
//!   __wbindgen_add_to_stack_pointer(delta) -> sp
//!
//! `wasm_solve` writes a status i32 at `retptr` and the answer as f64 at
//! `retptr + 8`. Status 0 means "no solution within the search budget".

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use wasmi::{Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

const WASM: &[u8] = include_bytes!("../../assets/deepseek_hash_v1.wasm");

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PowChallenge {
    #[serde(default)]
    pub algorithm: String,
    #[serde(default)]
    pub challenge: String,
    #[serde(default)]
    pub salt: String,
    #[serde(default)]
    pub signature: String,
    #[serde(default)]
    pub difficulty: u64,
    #[serde(default)]
    pub expire_at: u64,
    #[serde(default)]
    pub target_path: String,
}

/// Returns the base64 payload for the `x-ds-pow-response` header.
pub fn solve(ch: &PowChallenge) -> anyhow::Result<String> {
    if ch.salt.is_empty() {
        anyhow::bail!("pow: challenge has no salt");
    }
    let answer = solve_nonce(&ch.challenge, &ch.salt, ch.expire_at, ch.difficulty)?;
    tracing::debug!("pow solved: answer={answer} difficulty={}", ch.difficulty);
    Ok(encode(ch, answer))
}

/// (ret_ptr, challenge_ptr, challenge_len, prefix_ptr, prefix_len, difficulty)
type SolveI32 = TypedFunc<(i32, i32, i32, i32, i32, i32), ()>;
/// Same as SolveI32 but with the difficulty passed as f64.
type SolveF64 = TypedFunc<(i32, i32, i32, i32, i32, f64), ()>;

struct Wasm {
    store: Store<()>,
    memory: Memory,
    alloc: TypedFunc<(i32, i32), i32>,
    adjust: TypedFunc<i32, i32>,
    solve_i32: Option<SolveI32>,
    solve_f64: Option<SolveF64>,
}

impl Wasm {
    fn new() -> anyhow::Result<Self> {
        let engine = Engine::default();
        let module = Module::new(&engine, WASM)?;
        let mut store = Store::new(&engine, ());
        let linker = Linker::<()>::new(&engine);
        let instance: Instance = linker.instantiate_and_start(&mut store, &module)?;

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| anyhow::anyhow!("pow wasm: no memory export"))?;
        let alloc = instance.get_typed_func::<(i32, i32), i32>(&store, "__wbindgen_export_0")?;
        let adjust =
            instance.get_typed_func::<i32, i32>(&store, "__wbindgen_add_to_stack_pointer")?;

        // difficulty may be typed as u32 or f64 depending on the build
        let solve_i32 =
            instance.get_typed_func::<(i32, i32, i32, i32, i32, i32), ()>(&store, "wasm_solve").ok();
        let solve_f64 = if solve_i32.is_none() {
            instance.get_typed_func::<(i32, i32, i32, i32, i32, f64), ()>(&store, "wasm_solve").ok()
        } else {
            None
        };
        if solve_i32.is_none() && solve_f64.is_none() {
            anyhow::bail!("pow wasm: wasm_solve has an unexpected signature");
        }

        Ok(Self {
            store,
            memory,
            alloc,
            adjust,
            solve_i32,
            solve_f64,
        })
    }

    fn write(&mut self, s: &str) -> anyhow::Result<(i32, i32)> {
        let bytes = s.as_bytes();
        let ptr = self.alloc.call(&mut self.store, (bytes.len() as i32, 1))?;
        self.memory.write(&mut self.store, ptr as usize, bytes)?;
        Ok((ptr, bytes.len() as i32))
    }

    fn run(&mut self, challenge: &str, prefix: &str, difficulty: u64) -> anyhow::Result<Option<u64>> {
        let ch = self.write(challenge)?;
        let pf = self.write(prefix)?;

        let ret = self.adjust.call(&mut self.store, -16)?;
        match (&self.solve_i32, &self.solve_f64) {
            (Some(f), _) => {
                let f = *f;
                f.call(
                    &mut self.store,
                    (ret, ch.0, ch.1, pf.0, pf.1, difficulty as i32),
                )?;
            }
            (None, Some(f)) => {
                let f = *f;
                f.call(
                    &mut self.store,
                    (ret, ch.0, ch.1, pf.0, pf.1, difficulty as f64),
                )?;
            }
            (None, None) => anyhow::bail!("pow wasm: no usable wasm_solve"),
        }

        let mut status = [0u8; 4];
        self.memory.read(&self.store, ret as usize, &mut status)?;
        let status = i32::from_le_bytes(status);

        let mut value = [0u8; 8];
        self.memory
            .read(&self.store, (ret + 8) as usize, &mut value)?;
        let value = f64::from_le_bytes(value);

        self.adjust.call(&mut self.store, 16)?;

        if status == 0 || !value.is_finite() || value < 0.0 {
            return Ok(None);
        }
        Ok(Some(value.floor() as u64))
    }
}

fn solve_nonce(
    challenge: &str,
    salt: &str,
    expire_at: u64,
    difficulty: u64,
) -> anyhow::Result<u64> {
    let mut wasm = Wasm::new()?;
    let prefix = format!("{salt}_{expire_at}_");
    wasm.run(challenge, &prefix, difficulty)?
        .ok_or_else(|| anyhow::anyhow!("pow: solver returned no answer"))
}

fn encode(ch: &PowChallenge, answer: u64) -> String {
    let algorithm = if ch.algorithm.is_empty() {
        "DeepSeekHashV1".to_string()
    } else {
        ch.algorithm.clone()
    };
    let payload = serde_json::json!({
        "algorithm": algorithm,
        "challenge": ch.challenge,
        "salt": ch.salt,
        "answer": answer,
        "signature": ch.signature,
        "target_path": ch.target_path,
    });
    base64::engine::general_purpose::STANDARD.encode(payload.to_string())
}
