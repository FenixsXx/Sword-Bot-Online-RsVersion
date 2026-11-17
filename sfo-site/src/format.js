export function startOfWeek(date) {
  const result = new Date(date);
  const day = result.getDay();
  result.setDate(result.getDate() - (day === 0 ? 6 : day - 1));
  result.setHours(0, 0, 0, 0);
  return result;
}

export function dateKey(date) {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
}

export function formatDate(date, time) {
  const value = new Date(`${date}T${time}`);
  const options = { day: "2-digit", month: "short" };
  if (value.getFullYear() !== new Date().getFullYear()) options.year = "numeric";
  return `${value.toLocaleDateString("it-IT", options)} · ${time}`;
}

export function formatDateTime(isoValue) {
  const value = new Date(isoValue);
  if (Number.isNaN(value.getTime())) return String(isoValue ?? "—");
  const options = { day: "2-digit", month: "short", hour: "2-digit", minute: "2-digit" };
  if (value.getFullYear() !== new Date().getFullYear()) options.year = "numeric";
  return value.toLocaleString("it-IT", options);
}

export function formatNumber(value, digits = 1) {
  return Number(value).toLocaleString("it-IT", { minimumFractionDigits: digits, maximumFractionDigits: digits });
}

export function formatDuration(minutes) {
  const hours = Math.floor(minutes / 60);
  const mins = minutes % 60;
  return hours ? `${hours}h ${mins ? `${mins}m` : ""}`.trim() : `${mins}m`;
}

export function formatSeconds(seconds) {
  const total = Math.max(0, Math.round(Number(seconds) || 0));
  if (total < 60) return `${total}s`;
  return formatDuration(Math.round(total / 60));
}

export function pluralize(count, singular, plural) {
  return `${count} ${count === 1 ? singular : plural}`;
}
