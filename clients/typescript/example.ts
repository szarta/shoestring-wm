/**
 * Tiny demo of the shoestring-wm TypeScript client.
 *
 * Run inside a shoestring-wm session (so $SHOESTRING_WM_SOCKET is set):
 *
 *   npm install && npm run build && node dist/example.js
 *
 * or, with a TS runner: `npx tsx example.ts`.
 */

import { Client } from "./shoestring-ipc.js";

async function main(): Promise<void> {
  const wm = new Client();
  try {
    const ws = await wm.workspaces();
    console.log(`active workspace: ${ws.active} of ${ws.count}`);

    const { windows } = await wm.windows();
    console.log(`${windows.length} window(s):`);
    for (const win of windows) {
      console.log(`  [${win.workspace}] ${win.app_id}: ${win.title}`);
    }

    console.log("watching events (Ctrl-C to stop)...");
    for await (const event of wm.events()) {
      console.log("  event:", event);
    }
  } finally {
    wm.close();
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
