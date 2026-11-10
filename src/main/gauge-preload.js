'use strict';

const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('meter', {
  onData: (fn) => ipcRenderer.on('gauge:data', (_e, data) => fn(data)),
  refresh: () => ipcRenderer.send('gauge:refresh'),
  hide: () => ipcRenderer.send('gauge:hide'),
  pin: () => ipcRenderer.send('gauge:pin'),
  openClaude: () => ipcRenderer.send('gauge:claude'),
});
