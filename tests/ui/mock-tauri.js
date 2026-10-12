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
    // The right-click menu setting, answered here for every scenario.
    integration: { supported: true, installed: false, label: "Compress with zipc" },
    integrationError: null,
    // The current window: `closed` once a close went through.
    closed: false,
    closeHandler: null,
  };

  // Like Tauri: close() asks the close-requested handler, which may veto it.
  async function requestClose() {
    let prevented = false;
    if (mock.closeHandler) {
      await mock.closeHandler({ preventDefault: () => (prevented = true) });
    }
    if (!prevented) mock.closed = true;
    return prevented;
  }
  mock.requestClose = requestClose;

  function integrationCommand(cmd, args) {
    if (cmd === "integration_status") return Promise.resolve({ ...mock.integration });
    if (mock.integrationError) return Promise.reject(mock.integrationError);
    mock.integration.installed = args.enabled;
    return Promise.resolve({ ...mock.integration });
  }

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
        if (cmd === "integration_status" || cmd === "set_integration") return integrationCommand(cmd, args);
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
    window: {
      getCurrentWindow: () => ({
        close: () => {
          mock.calls.push(["close"]);
          return requestClose();
        },
        onCloseRequested: async (handler) => {
          mock.closeHandler = handler;
          return () => {};
        },
      }),
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
