/**
 * Bespoke Kinetic Vortex Turbine Brand Mark (`P1.5-T07`).
 *
 * Uses `useId()` for collision-free SVG gradient IDs and animates smoothly only
 * when `spinning` is true (automatically halted under `prefers-reduced-motion`).
 */

import { useId } from "react";

interface VortexLogoProps {
  readonly size?: number;
  readonly spinning?: boolean;
  readonly className?: string;
}

export function VortexLogo({ size = 32, spinning = false, className = "" }: VortexLogoProps) {
  const uid = useId().replace(/:/g, "");
  const bgId = `vtx-bg-${uid}`;
  const rimId = `vtx-rim-${uid}`;
  const bladeAId = `vtx-ba-${uid}`;
  const bladeBId = `vtx-bb-${uid}`;
  const coreId = `vtx-core-${uid}`;

  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      viewBox="0 0 512 512"
      width={size}
      height={size}
      fill="none"
      aria-hidden="true"
      className={`vtx-logo-svg ${className}`.trim()}
    >
      <defs>
        <linearGradient id={bgId} x1="48" y1="32" x2="464" y2="480" gradientUnits="userSpaceOnUse">
          <stop offset="0%" stopColor="#1E2438" />
          <stop offset="52%" stopColor="#121624" />
          <stop offset="100%" stopColor="#0B0E18" />
        </linearGradient>
        <linearGradient
          id={rimId}
          x1="256"
          y1="16"
          x2="256"
          y2="496"
          gradientUnits="userSpaceOnUse"
        >
          <stop offset="0%" stopColor="#A5B4FC" stopOpacity="0.48" />
          <stop offset="50%" stopColor="#6366F1" stopOpacity="0.18" />
          <stop offset="100%" stopColor="#22D3EE" stopOpacity="0.32" />
        </linearGradient>
        <linearGradient
          id={bladeAId}
          x1="0%"
          y1="100%"
          x2="100%"
          y2="0%"
          gradientUnits="objectBoundingBox"
        >
          <stop offset="0%" stopColor="#4338CA" />
          <stop offset="42%" stopColor="#6366F1" />
          <stop offset="78%" stopColor="#22D3EE" />
          <stop offset="100%" stopColor="#CFFAFE" />
        </linearGradient>
        <linearGradient
          id={bladeBId}
          x1="0%"
          y1="100%"
          x2="100%"
          y2="0%"
          gradientUnits="objectBoundingBox"
        >
          <stop offset="0%" stopColor="#4F46E5" />
          <stop offset="44%" stopColor="#818CF8" />
          <stop offset="80%" stopColor="#38BDF8" />
          <stop offset="100%" stopColor="#A5F3FC" />
        </linearGradient>
        <linearGradient
          id={coreId}
          x1="0%"
          y1="0%"
          x2="100%"
          y2="100%"
          gradientUnits="objectBoundingBox"
        >
          <stop offset="0%" stopColor="#CFFAFE" />
          <stop offset="48%" stopColor="#22D3EE" />
          <stop offset="100%" stopColor="#6366F1" />
        </linearGradient>
      </defs>

      <rect x="20" y="20" width="472" height="472" rx="112" fill={`url(#${bgId})`} />
      <rect
        x="24"
        y="24"
        width="464"
        height="464"
        rx="108"
        stroke={`url(#${rimId})`}
        strokeWidth="8"
      />

      <g transform="translate(256 256)">
        <g className={spinning ? "vtx-turbine-spin" : ""}>
          <path
            d="M -18 -56 C -8 -126, 66 -174, 146 -152 C 102 -128, 72 -96, 60 -42 C 52 -6, 32 16, 4 22 C 26 -6, 20 -36, -18 -56 Z"
            fill={`url(#${bladeAId})`}
          />
          <path
            transform="rotate(120)"
            d="M -18 -56 C -8 -126, 66 -174, 146 -152 C 102 -128, 72 -96, 60 -42 C 52 -6, 32 16, 4 22 C 26 -6, 20 -36, -18 -56 Z"
            fill={`url(#${bladeBId})`}
          />
          <path
            transform="rotate(240)"
            d="M -18 -56 C -8 -126, 66 -174, 146 -152 C 102 -128, 72 -96, 60 -42 C 52 -6, 32 16, 4 22 C 26 -6, 20 -36, -18 -56 Z"
            fill={`url(#${bladeAId})`}
          />
        </g>
        <circle cx="0" cy="0" r="54" fill="#0E1320" stroke={`url(#${coreId})`} strokeWidth="10" />
        <path
          d="M 0 -24 L 0 18 M -18 2 L 0 21 L 18 2"
          stroke="#E0F2FE"
          strokeWidth="12"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </g>
    </svg>
  );
}
