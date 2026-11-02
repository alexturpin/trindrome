// Compiles crates/trindrome-wasm and generates its JavaScript bindings in src/wasm.
// Needs the wasm32-unknown-unknown target and wasm-bindgen-cli matching the
// crate's pinned wasm-bindgen version.
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "../../..");
const run = (cmd, args) => execFileSync(cmd, args, { cwd: root, stdio: "inherit" });

run("cargo", ["build", "-p", "trindrome-wasm", "--target", "wasm32-unknown-unknown", "--release"]);
run("wasm-bindgen", [
  "--target", "web",
  "--out-dir", path.join(here, "../src/wasm"),
  path.join(root, "target/wasm32-unknown-unknown/release/trindrome_wasm.wasm"),
]);
