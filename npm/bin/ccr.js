#!/usr/bin/env node
'use strict';

const path = require('path');
const fs = require('fs');
const { spawn } = require('child_process');

// 平台二进制由 postinstall 下载到 vendor/<platform>-<arch>/ccr
const key = `${process.platform}-${process.arch}`;
const candidates = [];

if (process.env.CCR_BINARY_PATH) {
  candidates.push(process.env.CCR_BINARY_PATH);
}
candidates.push(path.join(__dirname, '..', 'vendor', key, 'ccr'));

const bin = candidates.find((p) => {
  try {
    return fs.statSync(p).isFile();
  } catch {
    return false;
  }
});

if (!bin) {
  console.error(
    `ccr: 未找到适用于 ${key} 的二进制文件。\n\n` +
      `  重新安装：npm i -g cc-remote-usage --force\n` +
      `  手动下载：https://github.com/FrankieeW/cc-remote-usage/releases\n` +
      `  放置位置：${path.join(__dirname, '..', 'vendor', key, 'ccr')}\n`
  );
  process.exit(1);
}

const child = spawn(bin, process.argv.slice(2), {
  stdio: 'inherit',
  // 让 ccr 直接继承终端的 TTY，交互式多选才正常
  env: process.env,
});

child.on('error', (err) => {
  console.error(`ccr: 启动失败：${err.message}`);
  process.exit(1);
});

child.on('exit', (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code === null ? 1 : code);
});
