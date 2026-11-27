/**
 * Lightweight glowing vector speed sparkline for the Top Command Bar Live Telemetry Pill (`P1.5-T08`).
 */

import { useId } from "react";

interface SpeedSparklineProps {
  readonly samples: readonly number[];
  readonly width?: number;
  readonly height?: number;
}

export function SpeedSparkline({ samples, width = 76, height = 22 }: SpeedSparklineProps) {
  const gradId = useId().replace(/:/g, "");
  const padded = samples.length >= 2 ? samples : [0, ...samples, 0];
  const maxVal = Math.max(1, ...padded);
  const stepX = width / Math.max(1, padded.length - 1);

  const coords = padded.map((val, idx) => {
    const x = idx * stepX;
    const norm = Math.max(0, Math.min(1, val / maxVal));
    const y = height - 2 - norm * (height - 5);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  });

  const polylinePoints = coords.join(" ");
  const areaPath = `M 0,${height} L ${coords.join(" L ")} L ${width},${height} Z`;

  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      aria-hidden="true"
      className="vtx-sparkline"
    >
      <defs>
        <linearGradient id={gradId} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="var(--vtx-brand-cyan)" stopOpacity="0.45" />
          <stop offset="100%" stopColor="var(--vtx-brand-primary)" stopOpacity="0.02" />
        </linearGradient>
      </defs>
      <path d={areaPath} fill={`url(#${gradId})`} />
      <polyline
        fill="none"
        stroke="var(--vtx-brand-cyan)"
        strokeWidth="1.75"
        strokeLinecap="round"
        strokeLinejoin="round"
        points={polylinePoints}
      />
    </svg>
  );
}
