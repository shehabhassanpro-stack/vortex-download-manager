/**
 * Pure formatting helpers for byte sizes, transfer speeds, and ETAs (`P1-T32`).
 */

export function formatBytes(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || bytes < 0) {
    return "—";
  }
  if (bytes === 0) {
    return "0 B";
  }
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let idx = 0;
  while (value >= 1024 && idx < units.length - 1) {
    value /= 1024;
    idx += 1;
  }
  const unit = units[idx] ?? "B";
  return `${value >= 100 || idx === 0 ? value.toFixed(0) : value.toFixed(2)} ${unit}`;
}

/**
 * Formats byte sizes with IEC binary (`GiB`) + SI decimal (`GB`) clarity for large files (`>= 100 MiB`),
 * eliminating user confusion when web hosts display SI decimal GB vs Windows binary GiB (`UX-M09`).
 */
export function formatBytesDual(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || bytes < 0) {
    return "—";
  }
  if (bytes < 100 * 1024 * 1024) {
    return formatBytes(bytes);
  }
  const iecUnits = ["B", "KiB", "MiB", "GiB", "TiB"];
  let iecVal = bytes;
  let iecIdx = 0;
  while (iecVal >= 1024 && iecIdx < iecUnits.length - 1) {
    iecVal /= 1024;
    iecIdx += 1;
  }
  const siUnits = ["B", "KB", "MB", "GB", "TB"];
  let siVal = bytes;
  let siIdx = 0;
  while (siVal >= 1000 && siIdx < siUnits.length - 1) {
    siVal /= 1000;
    siIdx += 1;
  }
  const iecUnit = iecUnits[iecIdx] ?? "GiB";
  const siUnit = siUnits[siIdx] ?? "GB";
  const iecStr = `${iecVal >= 100 ? iecVal.toFixed(0) : iecVal.toFixed(2)} ${iecUnit}`;
  const siStr = `${siVal >= 100 ? siVal.toFixed(0) : siVal.toFixed(2)} ${siUnit}`;
  return `${iecStr} (${siStr})`;
}

export function formatSpeed(bps: number): string {
  if (!Number.isFinite(bps) || bps <= 0) {
    return "—";
  }
  return `${formatBytes(bps)}/s`;
}

export function formatEta(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined || !Number.isFinite(seconds) || seconds <= 0) {
    return "—";
  }
  const mins = Math.floor(seconds / 60);
  const secs = Math.floor(seconds % 60);
  if (mins >= 60) {
    const hrs = Math.floor(mins / 60);
    const remMins = mins % 60;
    return `${String(hrs).padStart(2, "0")}:${String(remMins).padStart(2, "0")}:${String(secs).padStart(2, "0")}`;
  }
  return `${String(mins).padStart(2, "0")}:${String(secs).padStart(2, "0")}`;
}
