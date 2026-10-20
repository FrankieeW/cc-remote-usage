#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const https = require('https');
const { execFileSync } = require('child_process');

const REPO = 'FrankieeW/cc-remote-usage';

const TARGETS = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64': 'x86_64-apple-darwin',
  'linux-x64': 'x86_64-unknown-linux-musl',
  'linux-arm64': 'aarch64-unknown-linux-musl',
};

const key = `${process.platform}-${process.arch}`;
const triple = TARGETS[key];
const vendorDir = path.join(__dirname, '..', 'vendor', key);
const dest = path.join(vendorDir, 'ccr');

// 本地/离线场景：CCR_LOCAL_BINARY=/path/to/ccr npm i
function useLocalBinary() {
  const local = process.env.CCR_LOCAL_BINARY;
  if (!local) return false;
  fs.mkdirSync(vendorDir, { recursive: true });
  fs.copyFileSync(local, dest);
  fs.chmodSync(dest, 0o755);
  console.log(`ccr: 使用本地二进制 ${local}`);
  return true;
}

if (!triple) {
  console.error(
    `ccr: 暂不支持 ${key}。\n  请从 https://github.com/${REPO}/releases 手动下载对应二进制。`
  );
  process.exit(0); // 不阻断安装
}

if (useLocalBinary()) process.exit(0);

const version = require(path.join(__dirname, '..', 'package.json')).version;
const url = `https://github.com/${REPO}/releases/download/v${version}/ccr-${triple}`;

function download(u, redirects = 0) {
  return new Promise((resolve, reject) => {
    const req = https.get(
      u,
      { headers: { 'user-agent': 'cc-remote-usage-postinstall' }, timeout: 30000 },
      (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location && redirects < 5) {
          res.resume();
          resolve(download(res.headers.location, redirects + 1));
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`HTTP ${res.statusCode}`));
          return;
        }
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => resolve(Buffer.concat(chunks)));
        res.on('error', reject);
      }
    );
    req.on('timeout', () => req.destroy(new Error('下载超时')));
    req.on('error', reject);
  });
}

(async () => {
  try {
    const buf = await download(url);
    if (buf.length < 100 * 1024) {
      throw new Error(`下载内容异常（${buf.length} 字节）`);
    }
    fs.mkdirSync(vendorDir, { recursive: true });
    fs.writeFileSync(dest, buf);
    fs.chmodSync(dest, 0o755);

    // 自检：二进制能跑且有正确版本号
    const out = execFileSync(dest, ['--version'], { encoding: 'utf8' }).trim();
    if (!/^ccr \d+\.\d+\.\d+/.test(out)) {
      throw new Error(`二进制自检失败：${out}`);
    }
    console.log(`ccr: 已安装 ${out} (${triple})`);
  } catch (e) {
    console.error(`ccr: 自动下载二进制失败（${e.message}）。`);
    console.error(`  可手动下载：${url}`);
    console.error(`  放置到：${dest}  然后执行 chmod +x ${dest}`);
    // 不阻断 npm install；首次运行 ccr 时仍会给出同样提示
  }
})();
