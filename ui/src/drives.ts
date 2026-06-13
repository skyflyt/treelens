/**
 * Drive picker modal: lists logical drives with capacity bars.
 *
 * Falls back to a system folder picker (Tauri dialog plugin) for the "pick folder"
 * option. Either way, the result is a path that becomes `scan_start`'s argument.
 */
import { open } from "@tauri-apps/plugin-dialog";
import { ipc, type DriveEntry } from "./ipc";
import { fmtBytes } from "./format";

export async function pickScanRoot(): Promise<string | null> {
  const drives = await ipc.listDrives().catch(() => [] as DriveEntry[]);
  return await showModal(drives);
}

function showModal(drives: DriveEntry[]): Promise<string | null> {
  return new Promise((resolve) => {
    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop";
    backdrop.innerHTML = `
      <div class="modal" role="dialog" aria-modal="true">
        <div class="modal-head">
          <div class="modal-title">Choose what to scan</div>
          <button class="btn ghost small" id="modal-close">✕</button>
        </div>
        <div class="modal-body" id="modal-drives"></div>
        <div class="modal-foot">
          <button class="btn" id="pick-folder">Pick folder…</button>
          <button class="btn" id="pick-cancel">Cancel</button>
        </div>
      </div>
    `;
    document.body.appendChild(backdrop);

    const driveBody = backdrop.querySelector("#modal-drives") as HTMLDivElement;
    if (drives.length === 0) {
      driveBody.innerHTML = `<div class="muted small" style="padding:14px">No drives reported. Try "Pick folder…".</div>`;
    }
    for (const d of drives) {
      const row = document.createElement("div");
      row.className = "drive-row";
      const used = d.total > 0 ? 1 - d.free / d.total : 0;
      row.innerHTML = `
        <div class="drive-letter">${escapeHtml(d.letter)}</div>
        <div>
          <div class="drive-label">${escapeHtml(d.label || (d.fs ? d.fs + " drive" : "Local drive"))}</div>
          <div class="drive-sub">${fmtBytes(d.total - d.free)} used of ${fmtBytes(d.total)} · ${escapeHtml(d.fs)}</div>
        </div>
        <div>
          <div class="capacity-bar"><div class="capacity-fill" style="width:${(used * 100).toFixed(1)}%"></div></div>
        </div>`;
      row.addEventListener("click", () => {
        cleanup();
        resolve(d.letter);
      });
      driveBody.appendChild(row);
    }

    const cleanup = () => {
      backdrop.remove();
      document.removeEventListener("keydown", onKey);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        cleanup();
        resolve(null);
      }
    };
    document.addEventListener("keydown", onKey);

    backdrop.querySelector("#modal-close")?.addEventListener("click", () => {
      cleanup();
      resolve(null);
    });
    backdrop.querySelector("#pick-cancel")?.addEventListener("click", () => {
      cleanup();
      resolve(null);
    });
    backdrop.querySelector("#pick-folder")?.addEventListener("click", async () => {
      const picked = await open({ directory: true, multiple: false });
      if (picked && typeof picked === "string") {
        cleanup();
        resolve(picked);
      }
    });
    backdrop.addEventListener("click", (e) => {
      if (e.target === backdrop) {
        cleanup();
        resolve(null);
      }
    });
  });
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
