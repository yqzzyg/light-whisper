/** 秒数 → `mm:ss`，超过一小时显示 `h:mm:ss`。 */
export function formatDuration(totalSeconds: number | null | undefined): string {
  const seconds = Number.isFinite(totalSeconds) ? Math.max(0, Math.floor(totalSeconds ?? 0)) : 0;
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  const pad = (value: number) => String(value).padStart(2, "0");
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(rest)}` : `${pad(minutes)}:${pad(rest)}`;
}
