/**
 * 耗时毫秒 → 秒数文本（不带单位）。
 * 界面上一律按秒展示：0.1 秒以上一位小数，更小的值两位小数，避免出现 `0.0`。
 * 非法值返回 `null`，由调用方决定占位符。
 */
export function formatLatencySeconds(ms: number | null | undefined): string | null {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return null;
  const seconds = ms / 1000;
  return seconds.toFixed(seconds >= 0.1 ? 1 : 2);
}

/** 秒数 → `mm:ss`，超过一小时显示 `h:mm:ss`。 */
export function formatDuration(totalSeconds: number | null | undefined): string {
  const seconds = Number.isFinite(totalSeconds) ? Math.max(0, Math.floor(totalSeconds ?? 0)) : 0;
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  const pad = (value: number) => String(value).padStart(2, "0");
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(rest)}` : `${pad(minutes)}:${pad(rest)}`;
}
