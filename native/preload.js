const ipcRenderer = require("electron").ipcRenderer;
window.electron = {
  ipcRenderer: {
    invoke: ipcRenderer.invoke.bind(ipcRenderer),
    on: ipcRenderer.on.bind(ipcRenderer),
  },
  isDev: process.env.NODE_ENV === "development",
  isExperimental: process.env.RADICLE_UPSTREAM_EXPERIMENTAL === "true",
};

// https://github.com/electron/electron/issues/2863#issuecomment-479186008
window.exports = {"__esModule": true};