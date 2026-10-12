// A stand-in for the Tauri JS API (window.__TAURI__) so src/main.js runs in a
// plain browser. Every call is recorded in `__mock.calls`; scenarios script the
// answers through `__mock.dialog` and `__mock.onInvoke`.
(() => {
  const mock = {
    calls: [],
    // Next answers for the dialogs, in order; `undefined` once empty.
    dialog: { open: [], save: [] },
    // (cmd, args) => Promise; set by each scenario.
    onInvoke: async (cmd) => {
      throw new Error(`unexpected invoke ${cmd}`);
    },
    dragHandler: null,
    errors: [],
  };

  class Channel {
    constructor() {
      this.onmessage = null;
    }
  }

  window.__TAURI__ = {
    core: {
      Channel,
      invoke: (cmd, args) => {
        mock.calls.push(["invoke", cmd, args]);
        return mock.onInvoke(cmd, args);
      },
    },
    dialog: {
      open: async (opts) => {
        mock.calls.push(["open", opts]);
        return mock.dialog.open.shift();
      },
      save: async (opts) => {
        mock.calls.push(["save", opts]);
        return mock.dialog.save.shift();
      },
    },
    opener: {
      revealItemInDir: async (path) => {
        mock.calls.push(["reveal", path]);
      },
    },
    webview: {
      getCurrentWebview: () => ({
        onDragDropEvent: async (handler) => {
          mock.dragHandler = handler;
          return () => {};
        },
      }),
    },
  };

  window.addEventListener("error", (e) => mock.errors.push(`error: ${e.message}`));
  window.addEventListener("unhandledrejection", (e) => mock.errors.push(`unhandled rejection: ${e.reason}`));
  const consoleError = console.error;
  console.error = (...args) => {
    mock.errors.push(`console.error: ${args.join(" ")}`);
    consoleError(...args);
  };

  window.__mock = mock;
})();
