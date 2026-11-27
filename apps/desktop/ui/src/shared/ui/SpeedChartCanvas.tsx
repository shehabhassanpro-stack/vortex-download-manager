/**
 * Live 60-Second `<canvas>` Transfer Speed Sparkline (`P1-T33` / `Vortex-Architecture-Design.md` §15.5).
 */

import { useEffect, useRef } from "react";
import { DARK_PALETTE, LIGHT_PALETTE } from "@vortex/design-tokens";

interface SpeedChartCanvasProps {
  readonly samplesBps: readonly number[];
  readonly theme: "light" | "dark";
  readonly ariaLabel: string;
}

export function SpeedChartCanvas({ samplesBps, theme, ariaLabel }: SpeedChartCanvasProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (canvas === null) return;
    const ctx = canvas.getContext("2d");
    if (ctx === null) return;

    const palette = theme === "dark" ? DARK_PALETTE : LIGHT_PALETTE;
    const dpr = window.devicePixelRatio || 1;
    const width = canvas.clientWidth || 480;
    const height = 72;
    canvas.width = Math.floor(width * dpr);
    canvas.height = Math.floor(height * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    ctx.clearRect(0, 0, width, height);
    ctx.fillStyle = theme === "dark" ? "rgba(255,255,255,0.04)" : "rgba(15,23,42,0.04)";
    ctx.fillRect(0, 0, width, height);

    if (samplesBps.length < 2) {
      return;
    }

    const maxSpeed = Math.max(1_048_576, ...samplesBps);
    const stepX = width / Math.max(1, samplesBps.length - 1);

    ctx.beginPath();
    samplesBps.forEach((val, idx) => {
      const x = idx * stepX;
      const ratio = Math.min(1, Math.max(0, val / maxSpeed));
      const y = height - 6 - ratio * (height - 14);
      if (idx === 0) {
        ctx.moveTo(x, y);
      } else {
        ctx.lineTo(x, y);
      }
    });
    ctx.strokeStyle = palette.statusActive;
    ctx.lineWidth = 2;
    ctx.stroke();
  }, [samplesBps, theme]);

  return (
    <canvas
      ref={canvasRef}
      style={{ width: "100%", height: "72px", borderRadius: "var(--vtx-radius-control)" }}
      role="img"
      aria-label={ariaLabel}
    />
  );
}
