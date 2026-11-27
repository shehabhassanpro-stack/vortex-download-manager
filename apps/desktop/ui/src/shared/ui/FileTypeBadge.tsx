/**
 * Semantic File-Type Badge with crisp `lucide-react` vector icons (`P1.5-T08` / `P1.5-T09`).
 */

import { Cpu, FileArchive, FileBox, FileText, Film, Image as ImageIcon, Music } from "lucide-react";

const EXECUTABLE_EXTS = new Set([
  "exe",
  "msi",
  "msix",
  "msp",
  "bat",
  "cmd",
  "ps1",
  "vbs",
  "vbe",
  "js",
  "jse",
  "hta",
  "scr",
  "cpl",
  "reg",
  "jar",
  "lnk",
  "com",
  "iso",
  "img",
]);

export function extractFileExtension(fileName: string): string {
  const clean = fileName.trim().replace(/\.+$/, "");
  const dot = clean.lastIndexOf(".");
  if (dot <= 0 || dot === clean.length - 1) {
    return "";
  }
  return clean.slice(dot + 1).toLowerCase();
}

export function isExecutableFilename(fileName: string): boolean {
  return EXECUTABLE_EXTS.has(extractFileExtension(fileName));
}

interface FileTypeBadgeProps {
  readonly categoryId: string;
  readonly fileName: string;
  readonly size?: "sm" | "md" | "lg";
}

export function FileTypeBadge({ categoryId, fileName, size = "md" }: FileTypeBadgeProps) {
  const ext = extractFileExtension(fileName);
  const iconPx = size === "lg" ? 22 : size === "sm" ? 15 : 18;
  const dim = size === "lg" ? 44 : size === "sm" ? 28 : 36;

  let tint = "var(--vtx-brand-subtle)";
  let fg = "var(--vtx-brand-primary)";
  let Icon = FileBox;

  if (categoryId === "video" || ["mp4", "mkv", "webm", "avi", "mov"].includes(ext)) {
    tint = "var(--vtx-tint-video)";
    fg = "#a78bfa";
    Icon = Film;
  } else if (categoryId === "audio" || ["mp3", "flac", "wav", "m4a", "opus"].includes(ext)) {
    tint = "var(--vtx-tint-audio)";
    fg = "#ec4899";
    Icon = Music;
  } else if (
    categoryId === "compressed" ||
    ["zip", "rar", "7z", "tar", "gz", "zst", "xz", "iso", "img"].includes(ext)
  ) {
    tint = "var(--vtx-tint-archive)";
    fg = "var(--vtx-status-warning)";
    Icon = FileArchive;
  } else if (categoryId === "programs" || EXECUTABLE_EXTS.has(ext)) {
    tint = "var(--vtx-tint-program)";
    fg = "var(--vtx-status-active)";
    Icon = Cpu;
  } else if (categoryId === "documents" || ["pdf", "doc", "docx", "epub", "txt"].includes(ext)) {
    tint = "var(--vtx-tint-document)";
    fg = "var(--vtx-status-success)";
    Icon = FileText;
  } else if (categoryId === "images" || ["png", "jpg", "jpeg", "webp", "svg"].includes(ext)) {
    tint = "var(--vtx-tint-image)";
    fg = "var(--vtx-brand-primary)";
    Icon = ImageIcon;
  }

  return (
    <span
      className="vtx-file-badge"
      aria-hidden="true"
      style={{
        inlineSize: `${dim}px`,
        blockSize: `${dim}px`,
        background: tint,
        color: fg,
      }}
    >
      <Icon size={iconPx} strokeWidth={1.9} />
    </span>
  );
}
