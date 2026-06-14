/**
 * Canvas treemap renderer.
 *
 * Receives a flat `Rect[]` (computed in Rust) and renders them with hover
 * highlight, click selection, double-click drill-in, and an optional age-heat
 * coloring mode. Pan/zoom of the static layout is intentionally omitted — the
 * UX is "click to drill into a directory" + animated transition.
 */
import { type Rect } from "./ipc";
import { type ColorMode, colorForRect } from "./colors";

export interface TreemapTheme {
  dark: boolean;
  borderColor: string;
  textColor: string;
  bgColor: string;
}

export interface TreemapInteractions {
  onHover(rect: Rect | null, x: number, y: number): void;
  onClick(rect: Rect): void;
  onDoubleClick(rect: Rect): void;
  onContextMenu(rect: Rect, x: number, y: number): void;
}

export interface RectNameLookup {
  (idx: number): string;
}

export class Treemap {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private rects: Rect[] = [];
  private rootIdx = 0;
  private theme: TreemapTheme;
  private mode: ColorMode = "type";
  private hoverIdx: number | null = null;
  private selectedIdx: number | null = null;
  private nameOf: RectNameLookup = () => "";
  private interactions: TreemapInteractions;
  private dpr = 1;
  private widthCss = 0;
  private heightCss = 0;
  // Offscreen cache of the static treemap (all rects + labels). Hover/selection
  // just blit this and stroke an overlay, so a mousemove no longer re-renders
  // thousands of rects, gradients, and text every frame.
  private baseCanvas: HTMLCanvasElement;
  private baseCtx: CanvasRenderingContext2D;
  private baseReady = false;
  // Snapshot of the previous layout, used for a brief crossfade on drill.
  private prevCanvas: HTMLCanvasElement;
  private prevReady = false;
  private animRaf = 0;

  constructor(canvas: HTMLCanvasElement, theme: TreemapTheme, interactions: TreemapInteractions) {
    this.canvas = canvas;
    this.theme = theme;
    this.interactions = interactions;
    const ctx = canvas.getContext("2d", { alpha: false });
    if (!ctx) throw new Error("2D context unavailable");
    this.ctx = ctx;
    this.baseCanvas = document.createElement("canvas");
    const bctx = this.baseCanvas.getContext("2d", { alpha: false });
    if (!bctx) throw new Error("2D context unavailable");
    this.baseCtx = bctx;
    this.prevCanvas = document.createElement("canvas");
    canvas.addEventListener("mousemove", this.handleMouseMove);
    canvas.addEventListener("mouseleave", this.handleMouseLeave);
    canvas.addEventListener("click", this.handleClick);
    canvas.addEventListener("dblclick", this.handleDblClick);
    canvas.addEventListener("contextmenu", this.handleContext);
    canvas.addEventListener("keydown", this.handleKeyDown);
  }

  setTheme(theme: TreemapTheme) {
    this.theme = theme;
    this.renderBase();
    this.draw();
  }
  setMode(mode: ColorMode) {
    this.mode = mode;
    this.renderBase();
    this.draw();
  }
  setSelected(idx: number | null) {
    this.selectedIdx = idx;
    this.draw();
  }
  setData(rects: Rect[], rootIdx: number, nameOf: RectNameLookup) {
    // Snapshot the current layout so we can crossfade into the new one.
    this.snapshotPrev();
    this.rects = rects;
    this.rootIdx = rootIdx;
    this.nameOf = nameOf;
    this.hoverIdx = null;
    this.renderBase();
    this.animateIn();
  }

  /** Copy the current base layer into the prev buffer for the next transition. */
  private snapshotPrev() {
    if (!this.baseReady || this.baseCanvas.width === 0) {
      this.prevReady = false;
      return;
    }
    this.prevCanvas.width = this.baseCanvas.width;
    this.prevCanvas.height = this.baseCanvas.height;
    const c = this.prevCanvas.getContext("2d", { alpha: false });
    if (!c) {
      this.prevReady = false;
      return;
    }
    c.setTransform(1, 0, 0, 1, 0, 0);
    c.drawImage(this.baseCanvas, 0, 0);
    this.prevReady = true;
  }

  private static reducedMotion(): boolean {
    return (
      typeof window !== "undefined" &&
      !!window.matchMedia &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches
    );
  }

  /** Crossfade + gentle zoom from the previous layout into the new one. Falls
   *  back to an instant draw when motion is reduced or there's nothing to fade
   *  from (first scan). */
  private animateIn() {
    if (this.animRaf) {
      cancelAnimationFrame(this.animRaf);
      this.animRaf = 0;
    }
    if (Treemap.reducedMotion() || !this.prevReady) {
      this.draw();
      return;
    }
    const ctx = this.ctx;
    const W = this.canvas.width;
    const H = this.canvas.height;
    const DUR = 190;
    const start = performance.now();
    const step = (now: number) => {
      const t = Math.min(1, (now - start) / DUR);
      const eased = t * (2 - t); // easeOutQuad
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      // Old layout underneath.
      ctx.globalAlpha = 1;
      ctx.drawImage(this.prevCanvas, 0, 0);
      // New layout fading in, easing from a slight zoom to 1:1.
      const scale = 1.05 - 0.05 * eased;
      const dw = W * scale;
      const dh = H * scale;
      ctx.globalAlpha = eased;
      ctx.drawImage(this.baseCanvas, (W - dw) / 2, (H - dh) / 2, dw, dh);
      ctx.globalAlpha = 1;
      if (t < 1) {
        this.animRaf = requestAnimationFrame(step);
      } else {
        this.animRaf = 0;
        this.prevReady = false;
        this.draw(); // settle to the crisp final frame + overlay
      }
    };
    this.animRaf = requestAnimationFrame(step);
  }

  /** Resize canvas backing store to match its CSS size and the device pixel ratio. */
  resize(cssWidth: number, cssHeight: number) {
    this.dpr = Math.max(1, Math.min(2.5, window.devicePixelRatio || 1));
    this.widthCss = cssWidth;
    this.heightCss = cssHeight;
    this.canvas.width = Math.floor(cssWidth * this.dpr);
    this.canvas.height = Math.floor(cssHeight * this.dpr);
    this.canvas.style.width = cssWidth + "px";
    this.canvas.style.height = cssHeight + "px";
    this.baseCanvas.width = this.canvas.width;
    this.baseCanvas.height = this.canvas.height;
    this.renderBase();
    this.draw();
  }

  hitTest(x: number, y: number): Rect | null {
    // Walk from end to start: the layout pushes deeper rects later, and we want
    // the smallest (deepest) rect at this point.
    for (let i = this.rects.length - 1; i >= 0; i--) {
      const r = this.rects[i];
      if (r.idx === this.rootIdx) continue;
      if (x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h) {
        return r;
      }
    }
    return null;
  }

  /** Render the static treemap (every rect + label) into the offscreen cache.
   *  Called only when the data, theme, mode, or size changes — never on hover. */
  private renderBase() {
    if (this.baseCanvas.width === 0 || this.baseCanvas.height === 0) {
      this.baseReady = false;
      return;
    }
    const ctx = this.baseCtx;
    ctx.save();
    ctx.scale(this.dpr, this.dpr);
    ctx.fillStyle = this.theme.bgColor;
    ctx.fillRect(0, 0, this.widthCss, this.heightCss);

    // Two passes: deeper rects on top, but we render in-order since layout is
    // already child-after-parent.
    for (const r of this.rects) {
      if (r.idx === this.rootIdx) continue;
      const name = r.name || this.nameOf(r.idx);
      const base = colorForRect(r, name, this.mode, { dark: this.theme.dark });
      ctx.fillStyle = base;
      ctx.fillRect(r.x, r.y, r.w, r.h);

      // Faint cushion-style inner shadow for the classic treemap depth cue, but cheap:
      // a single linear gradient across the rect rather than per-pixel cushion math.
      if (!r.is_dir && r.w > 4 && r.h > 4) {
        const g = ctx.createLinearGradient(r.x, r.y, r.x, r.y + r.h);
        const tint = this.theme.dark ? "rgba(255,255,255,0.06)" : "rgba(255,255,255,0.18)";
        const shade = this.theme.dark ? "rgba(0,0,0,0.18)" : "rgba(0,0,0,0.10)";
        g.addColorStop(0, tint);
        g.addColorStop(1, shade);
        ctx.fillStyle = g;
        ctx.fillRect(r.x, r.y, r.w, r.h);
      }

      // Border
      ctx.lineWidth = r.is_dir ? 1 : 0.5;
      ctx.strokeStyle = this.theme.borderColor;
      ctx.strokeRect(r.x + 0.5, r.y + 0.5, r.w - 1, r.h - 1);

      // Label: directory header for dirs with room, or a centered file label for big files.
      if (name) {
        if (r.is_dir && r.w > 50 && r.h > 18) {
          const padX = 4;
          const padY = 3;
          ctx.fillStyle = this.theme.dark ? "rgba(0,0,0,0.35)" : "rgba(255,255,255,0.55)";
          ctx.fillRect(r.x, r.y, r.w, 16);
          ctx.fillStyle = this.theme.dark ? "#e5e7eb" : "#1f2937";
          ctx.font = "600 11px ui-sans-serif, system-ui";
          ctx.textBaseline = "top";
          const text = clip(ctx, name, r.w - padX * 2);
          ctx.fillText(text, r.x + padX, r.y + padY);
        } else if (!r.is_dir && r.w > 60 && r.h > 22) {
          ctx.fillStyle = this.theme.dark ? "rgba(255,255,255,0.85)" : "rgba(255,255,255,0.95)";
          ctx.font = "500 10.5px ui-sans-serif, system-ui";
          ctx.textBaseline = "middle";
          const text = clip(ctx, name, r.w - 8);
          ctx.fillText(text, r.x + 4, r.y + r.h / 2);
        }
      }
    }
    ctx.restore();
    this.baseReady = true;
  }

  /** Composite the cached static layer + the hover/selection overlay. Cheap —
   *  this is what runs on every mousemove and selection change. */
  draw() {
    const ctx = this.ctx;
    // Blit the cached base in device pixels (1:1, no scaling).
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    if (this.baseReady) {
      ctx.drawImage(this.baseCanvas, 0, 0);
    } else {
      ctx.fillStyle = this.theme.bgColor;
      ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
    }

    ctx.save();
    ctx.scale(this.dpr, this.dpr);
    // Hover/selection overlay.
    if (this.hoverIdx !== null) {
      const r = this.rects.find((rr) => rr.idx === this.hoverIdx);
      if (r) {
        ctx.fillStyle = this.theme.dark ? "rgba(255,255,255,0.12)" : "rgba(0,0,0,0.07)";
        ctx.fillRect(r.x, r.y, r.w, r.h);
        ctx.lineWidth = 2;
        ctx.strokeStyle = this.theme.dark ? "#93c5fd" : "#1d4ed8";
        ctx.strokeRect(r.x + 1, r.y + 1, r.w - 2, r.h - 2);
      }
    }
    if (this.selectedIdx !== null && this.selectedIdx !== this.hoverIdx) {
      const r = this.rects.find((rr) => rr.idx === this.selectedIdx);
      if (r) {
        ctx.lineWidth = 2;
        ctx.strokeStyle = this.theme.dark ? "#fbbf24" : "#b45309";
        ctx.strokeRect(r.x + 1, r.y + 1, r.w - 2, r.h - 2);
      }
    }
    ctx.restore();
  }

  private handleMouseMove = (e: MouseEvent) => {
    const { left, top } = this.canvas.getBoundingClientRect();
    const x = e.clientX - left;
    const y = e.clientY - top;
    const r = this.hitTest(x, y);
    const newIdx = r?.idx ?? null;
    if (newIdx !== this.hoverIdx) {
      this.hoverIdx = newIdx;
      this.draw();
    }
    this.interactions.onHover(r, e.clientX, e.clientY);
  };

  private handleMouseLeave = () => {
    if (this.hoverIdx !== null) {
      this.hoverIdx = null;
      this.draw();
    }
    this.interactions.onHover(null, 0, 0);
  };

  private handleClick = (e: MouseEvent) => {
    const { left, top } = this.canvas.getBoundingClientRect();
    const r = this.hitTest(e.clientX - left, e.clientY - top);
    if (r) this.interactions.onClick(r);
  };

  private handleDblClick = (e: MouseEvent) => {
    const { left, top } = this.canvas.getBoundingClientRect();
    const r = this.hitTest(e.clientX - left, e.clientY - top);
    if (r) this.interactions.onDoubleClick(r);
  };

  private handleContext = (e: MouseEvent) => {
    e.preventDefault();
    const { left, top } = this.canvas.getBoundingClientRect();
    const r = this.hitTest(e.clientX - left, e.clientY - top);
    if (r) this.interactions.onContextMenu(r, e.clientX, e.clientY);
  };

  /** Arrow keys move the selection to the spatially nearest rect in that
   *  direction; Enter drills into the selected rect. Makes the treemap usable
   *  without a mouse. */
  private handleKeyDown = (e: KeyboardEvent) => {
    const dirs: Record<string, [number, number]> = {
      ArrowLeft: [-1, 0],
      ArrowRight: [1, 0],
      ArrowUp: [0, -1],
      ArrowDown: [0, 1],
    };
    if (e.key === "Enter") {
      const r = this.rects.find((rr) => rr.idx === this.selectedIdx);
      if (r) {
        e.preventDefault();
        this.interactions.onDoubleClick(r);
      }
      return;
    }
    const dir = dirs[e.key];
    if (!dir) return;
    e.preventDefault();
    const selectable = this.rects.filter((r) => r.idx !== this.rootIdx);
    if (selectable.length === 0) return;
    const cur = selectable.find((r) => r.idx === this.selectedIdx);
    if (!cur) {
      this.interactions.onClick(selectable[0]);
      return;
    }
    const cx = cur.x + cur.w / 2;
    const cy = cur.y + cur.h / 2;
    let best: Rect | null = null;
    let bestScore = Infinity;
    for (const r of selectable) {
      if (r.idx === cur.idx) continue;
      const rx = r.x + r.w / 2;
      const ry = r.y + r.h / 2;
      const dx = rx - cx;
      const dy = ry - cy;
      // Must lie predominantly in the requested direction.
      const along = dx * dir[0] + dy * dir[1];
      if (along <= 0) continue;
      const perp = Math.abs(dx * dir[1] + dy * dir[0]);
      // Prefer small perpendicular offset, then nearest along the axis.
      const score = along + perp * 2;
      if (score < bestScore) {
        bestScore = score;
        best = r;
      }
    }
    if (best) this.interactions.onClick(best);
  };
}

function clip(ctx: CanvasRenderingContext2D, text: string, maxW: number): string {
  if (ctx.measureText(text).width <= maxW) return text;
  const ell = "…";
  let lo = 0;
  let hi = text.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    const slice = text.slice(0, mid) + ell;
    if (ctx.measureText(slice).width <= maxW) lo = mid;
    else hi = mid - 1;
  }
  return lo > 0 ? text.slice(0, lo) + ell : "";
}
