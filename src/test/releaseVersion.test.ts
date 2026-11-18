import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";

const repoRoot = path.resolve(__dirname, "../..");

function readRepoFile(relativePath: string): string {
  return readFileSync(path.join(repoRoot, relativePath), "utf8");
}

function readJsonVersion(relativePath: string): string {
  return JSON.parse(readRepoFile(relativePath)).version;
}

function readTomlString(relativePath: string, key: string): string {
  const content = readRepoFile(relativePath);
  const match = content.match(new RegExp(`^${key}\\s*=\\s*"([^"]+)"`, "m"));
  if (!match) {
    throw new Error(`${relativePath} is missing ${key}`);
  }
  return match[1];
}

/**
 * 锁文件里的根包名来自各自的清单（Cargo package name / pyproject name），
 * 这里跟着清单读，不再写死 —— 产品更名时少一处必改的字符串。
 */
function readLockProjectVersion(
  lockPath: string,
  manifestPath: string,
  manifestLabel: string,
): string {
  const packageName = readTomlString(manifestPath, "name");
  const content = readRepoFile(lockPath);
  const match = content.match(
    new RegExp(`\\[\\[package\\]\\]\\r?\\nname = "${packageName}"\\r?\\nversion = "([^"]+)"`),
  );
  if (!match) {
    throw new Error(`${lockPath} is missing the ${packageName} root package from ${manifestLabel}`);
  }
  return match[1];
}

function readUvProjectVersion(): string {
  return readLockProjectVersion("uv.lock", "pyproject.toml", "pyproject.toml");
}

function readCargoLockProjectVersion(): string {
  return readLockProjectVersion("src-tauri/Cargo.lock", "src-tauri/Cargo.toml", "Cargo.toml");
}

describe("release metadata", () => {
  it("keeps project versions aligned across release manifests", () => {
    const versions = {
      "package.json": readJsonVersion("package.json"),
      "src-tauri/Cargo.toml": readTomlString("src-tauri/Cargo.toml", "version"),
      "src-tauri/tauri.conf.json": readJsonVersion("src-tauri/tauri.conf.json"),
      "pyproject.toml": readTomlString("pyproject.toml", "version"),
      "src-tauri/Cargo.lock": readCargoLockProjectVersion(),
      "uv.lock": readUvProjectVersion(),
    };

    expect(new Set(Object.values(versions)).size, JSON.stringify(versions, null, 2)).toBe(1);
  });

  it("keeps pyproject Python support aligned with the packaged runtime", () => {
    expect(readTomlString("pyproject.toml", "requires-python")).toBe(">=3.11, <3.13");
  });
});
