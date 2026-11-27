import { useEffect, useRef } from "react";
import { DARK_PALETTE, LIGHT_PALETTE } from "@vortex/design-tokens";

interface SegmentBarCanvasProps {
  readonly bins: Uint8Array;
  readonly theme: "light" | "dark";
  readonly ariaLabel: string;
}

/** 60 FPS `<canvas>` renderer for the 512-bin segment map (`Design-System.md` §4.2). */
export function SegmentBarCanvas({ bins, theme, ariaLabel }: SegmentBarCanvasProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const palette = theme === "dark" ? DARK_PALETTE : LIGHT_PALETTE;
    const dpr = window.devicePixelRatio || 1;
    const width = canvas.clientWidth || 512;
    const height = 14;
    canvas.width = Math.floor(width * dpr);
    canvas.height = Math.floor(height * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    ctx.fillStyle = theme === "dark" ? "rgba(255,255,255,0.08)" : "rgba(15,23,42,0.08)";
    ctx.fillRect(0, 0, width, height);

    const binWidth = width / bins.length;
    for (let i = 0; i < bins.length; i += 1) {
      const state = bins[i] ?? 0;
      if (state === 0) continue;
      ctx.fillStyle = state === 1 ? palette.brandPrimary : palette.statusActive;
      ctx.fillRect(i * binWidth, 0, Math.max(1, Math.ceil(binWidth)), height);
    }
  }, [bins, theme]);

  return <canvas ref={canvasRef} className="vtx-canvas-bar" role="img" aria-label={ariaLabel} />;
}
