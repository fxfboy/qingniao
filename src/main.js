/* 青鸟 · 飞书消息助手 — Tauri 前端（v2 原型落地版） */
(function(){
'use strict';

/* ===================== 工具 ===================== */
const $  = (s, r=document) => r.querySelector(s);
const $$ = (s, r=document) => Array.from(r.querySelectorAll(s));
const esc = s => String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const nowIso = () => new Date().toISOString();
const fmtSize = b => {
  b = Number(b) || 0;
  if (b >= 1073741824) return (b/1073741824).toFixed(2) + ' GB';
  if (b >= 1048576)    return (b/1048576).toFixed(1) + ' MB';
  if (b >= 1024)       return Math.round(b/1024) + ' KB';
  return b + ' B';
};
const fmtClock = d => ('0'+d.getHours()).slice(-2) + ':' + ('0'+d.getMinutes()).slice(-2);
const dayKey = iso => { const d = new Date(iso); return d.getFullYear()+'-'+d.getMonth()+'-'+d.getDate(); };
const dayLabel = iso => {
  const d = new Date(iso), now = new Date();
  const yest = new Date(now); yest.setDate(now.getDate()-1);
  const wd = ['周日','周一','周二','周三','周四','周五','周六'][d.getDay()];
  if (d.toDateString() === now.toDateString()) return '今天';
  if (d.toDateString() === yest.toDateString()) return '昨天';
  return (d.getMonth()+1) + '月' + d.getDate() + '日 · ' + wd;
};

/* ===================== Tauri 封装 ===================== */
const TAURI = window.__TAURI__ || null;
const invoke = TAURI ? TAURI.core.invoke : null;
if(!invoke){ console.warn('Tauri 不可用，部分功能将降级'); }
async function listenEvent(name, handler){
  if(!TAURI || !TAURI.event) return;
  try{ await TAURI.event.listen(name, handler); }catch(e){ console.error('监听失败:', name, e); }
}

/* ===================== 日志 ===================== */
async function log(level, msg){
  try{
    const line = '[青鸟] ' + msg;
    if(level === 'error') console.error(line);
    else if(level === 'warn') console.warn(line);
    else console.log(line);
  }catch(e){}
  if(invoke){
    try{ await invoke('log_event', {level, message: msg}); }
    catch(e){ console.error('写入日志文件失败:', e); }
  }
}
window.addEventListener('error', e => {
  const msg = e.message || (e.error && e.error.stack) || '未知错误';
  log('error', '未捕获异常: ' + msg);
});
window.addEventListener('unhandledrejection', e => {
  const r = e.reason;
  const msg = (r && (r.message || r.stack)) || String(r) || '未知原因';
  log('error', '未处理的 Promise 拒绝: ' + msg);
});

/* ===================== 状态 ===================== */
let config = null;
let hotkeyMode = 'mod';         // 'mod' | 'enter'
let pendingFile = null;         // 确认弹窗中的待发文件 {path, name, size}
let liveTasks = new Map();      // task_id -> {record, el}
let importFpOk = false;

/* ===================== 配置加载/保存 ===================== */
function defaultConfig(){
  return {
    schema_version: 2,
    webhooks: [], history: [], last_webhook: 0, last_type: 'auto',
    app_id: '', app_secret: '',
    theme: 'auto',
    transfer: { configured_port: 9876, download_dir: null }
  };
}
async function loadConfig(){
  if(invoke){
    try{ config = await invoke('load_config'); }
    catch(e){
      console.error('加载配置失败:', e);
      log('error', '加载配置失败: ' + (e.message || e));
      config = defaultConfig();
    }
  } else {
    config = defaultConfig();
  }
  if(!config.webhooks) config.webhooks = [];
  if(!config.history) config.history = [];
  if(config.last_webhook === undefined || config.last_webhook === null || config.last_webhook >= config.webhooks.length) config.last_webhook = 0;
  if(!config.last_type) config.last_type = 'auto';
  if(!config.app_id) config.app_id = '';
  if(!config.app_secret) config.app_secret = '';
  if(!config.theme) config.theme = 'auto';
  if(!config.transfer) config.transfer = { configured_port: 9876, download_dir: null };
  if(!config.transfer.configured_port) config.transfer.configured_port = 9876;
  // 旧版记录字段迁移：msg_type -> kind
  for(const h of config.history){
    if(!h.kind && h.msg_type) h.kind = h.msg_type;
    if(!h.kind) h.kind = 'post';
    if(!h.dir) h.dir = 'out';
  }
  hotkeyMode = config.hotkey === 'enter' ? 'enter' : 'mod';
}
let saveTimer = null;
function saveConfig(){
  // 短去抖合并写盘
  clearTimeout(saveTimer);
  saveTimer = setTimeout(async () => {
    if(invoke){
      try{ await invoke('save_config', {config}); }
      catch(e){ console.error('保存配置失败:', e); log('error', '保存配置失败: ' + (e.message || e)); }
    }
  }, 120);
}

/* ===================== Toast ===================== */
let toastTimer = null;
function toast(msg, meta, kind){
  const t = $('#toast');
  $('#toastText').textContent = msg;
  $('#toastMeta').textContent = meta || '';
  t.classList.toggle('err', kind === 'err');
  t.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.remove('show'), 2600);
}

/* ===================== 主题 ===================== */
const themeMql = window.matchMedia('(prefers-color-scheme: dark)');
function applyTheme(pref){
  const resolved = pref === 'dark' || pref === 'light' ? pref : (themeMql.matches ? 'dark' : 'light');
  document.documentElement.dataset.theme = resolved;
  document.documentElement.dataset.themePref = pref;
}
themeMql.addEventListener('change', () => {
  if((document.documentElement.dataset.themePref || 'auto') === 'auto') applyTheme('auto');
});
function initThemeCards(){
  $$('.theme-card').forEach(c => {
    const on = c.dataset.theme === (config.theme || 'auto');
    c.classList.toggle('active', on);
    c.setAttribute('aria-pressed', String(on));
    c.onclick = () => {
      $$('.theme-card').forEach(x => { x.classList.remove('active'); x.setAttribute('aria-pressed','false'); });
      c.classList.add('active'); c.setAttribute('aria-pressed','true');
      config.theme = c.dataset.theme;
      applyTheme(config.theme);
      saveConfig();
    };
  });
}

/* ===================== 编辑器（contenteditable） ===================== */
const ed = $('#editor');
const expEd = $('#expandEd');
const RANGES = new Map();

document.addEventListener('selectionchange', () => {
  const sel = window.getSelection();
  if (!sel || !sel.rangeCount) return;
  const r = sel.getRangeAt(0);
  [ed, expEd].forEach(el => { if (el.contains(r.commonAncestorContainer)) RANGES.set(el, r.cloneRange()); });
});

function hasChips(el){ return !!el.querySelector('.chip'); }
// 读取编辑器文本时排除图片 chip 内部的文件名/×按钮文字，避免被当作消息正文
function edText(el){
  const chips = $$('.chip', el);
  if (!chips.length) return el.innerText || '';
  const prev = chips.map(c => c.style.display);
  chips.forEach(c => { c.style.display = 'none'; });
  const t = el.innerText || '';
  chips.forEach((c, i) => { c.style.display = prev[i]; });
  return t;
}
function isBlank(el){ return (el.innerText||'').replace(/\u200b/g,'').trim() === '' && !hasChips(el); }
function syncEmpty(el){
  if((el.innerText||'').replace(/\u200b/g,'').trim() === '' && !hasChips(el)) el.innerHTML = '';
  el.classList.toggle('is-empty', isBlank(el));
}
function countChars(el){ return ((edText(el)||'').replace(/\s/g,'')).length; }
function activeEd(){ return $('#expand').classList.contains('show') ? expEd : ed; }

const TYPE_LABEL = { text:'文本', post:'富文本', image:'图片', interactive:'交互卡片' };
/* 类型识别走 Rust core（invoke 'analyze'，D15：150ms 防抖 + 递增序号，仅最新序号生效）；
 * Tauri 不可用时降级到 src/message.js 的本地 detectType（同一基线实现） */
let analyzeSeq = 0, analyzeTimer = null, lastAnalysis = { t:'text', why:'' };
function analyzeAsync(){
  const seq = ++analyzeSeq;
  const el = activeEd();
  const text = edText(el);
  const chips = $$('.chip', el).length;
  const apply = (d) => {
    if (seq !== analyzeSeq) return;   // 旧响应不得覆盖新识别
    lastAnalysis = d;
    renderTypePill(d);
  };
  if(!invoke){ apply(detectType(text, chips)); return; }
  invoke('analyze', {text, chips}).then(apply).catch(() => apply(detectType(text, chips)));
}
function detect(el){
  // 保留同步取值口径（send 校验等用 lastAnalysis），识别更新走 analyzeAsync
  return lastAnalysis;
}
function renderTypePill(d){
  const el = activeEd();
  const ft = forcedType();
  const finalType = ft === 'auto' ? d.t : ft;
  const pill = $('#typePill');
  pill.className = 'dpill ' + (finalType === 'interactive' ? 'interactive' : finalType);
  pill.textContent = TYPE_LABEL[finalType] || '文本';
  $('#whyText').innerHTML = ft === 'auto' ? d.why :
    '已指定为 <b>' + TYPE_LABEL[ft] + '</b>（识别结果：' + TYPE_LABEL[d.t] + '）';
}
function forcedType(){
  const v = config.last_type || 'auto';
  return v === 'auto' || TYPE_LABEL[v] ? v : 'auto';
}
function refresh(){
  const el = activeEd();
  renderTypePill(lastAnalysis);

  const n = countChars(el);
  $('#counter').textContent = n ? n + ' 字' : '';
  $('#expandCount').textContent = n + ' 字';

  const ready = !isBlank(el);
  [$('#btnSend'), $('#expandSend')].forEach(b => {
    b.classList.toggle('ready', ready);
    b.setAttribute('aria-disabled', String(!ready));
  });

  // 输入防抖 150ms 后异步识别（D15）
  clearTimeout(analyzeTimer);
  analyzeTimer = setTimeout(analyzeAsync, 150);
  if (analyzeTimer && !ready) { /* 空编辑器也走防抖，保证 pill 复位 */ }
}
function updateBotLabels(){
  const bot = config.webhooks[config.last_webhook];
  const name = bot ? bot.name : '未设置';
  $('#confirmTarget').textContent = name;
  $('#convName').textContent = bot ? bot.name : '消息记录';
  ed.setAttribute('data-ph', '发送给 ' + name);
  ed.setAttribute('aria-label', '发送给 ' + name);
}
[ed, expEd].forEach(el => {
  el.addEventListener('input', ev => {
    if (ev && ev.isComposing){ refresh(); return; }
    syncEmpty(el); refresh();
  });
  el.addEventListener('blur', () => syncEmpty(el));
  el.addEventListener('keydown', e => {
    if (e.key !== 'Enter') return;
    if (e.shiftKey) return;
    const sendOnEnter = hotkeyMode === 'enter' ? !e.isComposing : (e.metaKey || e.ctrlKey);
    if (sendOnEnter){ e.preventDefault(); send(); }
  });
  el.addEventListener('paste', e => {
    const items = e.clipboardData && e.clipboardData.items;
    if (!items) return;
    for (const it of items){
      if (it.type && it.type.indexOf('image/') === 0){
        e.preventDefault();
        const f = it.getAsFile();
        const r = new FileReader();
        r.onload = ev => insertChip(el, f && f.name ? f.name : '粘贴的图片.png', ev.target.result);
        r.readAsDataURL(f);
      }
    }
  });
});

function insertNode(el, node){
  const r = RANGES.get(el);
  if (r && el.contains(r.commonAncestorContainer)){
    r.deleteContents(); r.insertNode(node);
    const after = document.createRange();
    after.setStartAfter(node); after.collapse(true);
    RANGES.set(el, after);
  } else {
    el.appendChild(node);
  }
  el.focus();
  if (RANGES.get(el)){
    const sel = window.getSelection();
    sel.removeAllRanges(); sel.addRange(RANGES.get(el));
  }
  syncEmpty(el); refresh();
}
const TONES = ['', 'b', 'c'];
/* 输入框内的图片附件：直接放 <img> 大图预览（对标飞书输入框）。
 * 之前用 44px 方形 background-image，截图这种宽图被 cover 裁得只剩中间一小块，
 * 等于看不见；这里按原图比例定尺寸，完整显示 */
const CHIP_MAX_W = 200, CHIP_MAX_H = 140, CHIP_MIN = 44;
function sizeChipThumb(img){
  const w = img.naturalWidth, h = img.naturalHeight;
  if (!w || !h) return;
  const s = Math.min(CHIP_MAX_W / w, CHIP_MAX_H / h, 1);
  img.style.width = Math.max(CHIP_MIN, Math.round(w * s)) + 'px';
  img.style.height = Math.max(CHIP_MIN, Math.round(h * s)) + 'px';
}
function insertChip(el, name, dataUrl){
  const chip = document.createElement('span');
  chip.className = 'chip';
  chip.setAttribute('contenteditable', 'false');
  chip.setAttribute('data-name', name);
  chip.innerHTML = '<img class="cimg" alt="" draggable="false">' +
                   '<span class="cnm"></span>' +
                   '<button class="cx" type="button" aria-label="移除这张图片">×</button>';
  chip.querySelector('.cnm').textContent = name;
  const img = chip.querySelector('.cimg');
  if (dataUrl){
    img.onload = () => sizeChipThumb(img);
    img.onerror = () => { img.classList.add('bad'); chip.title = '这张图无法预览（仍会按原图发出）'; };
    img.src = dataUrl;
    if (img.complete) sizeChipThumb(img);   // 已解码完成时 load 不会再触发
  } else {
    img.classList.add('bad');
  }
  chip._dataUrl = dataUrl || '';
  insertNode(el, chip);
  uploadChip(chip);
  return chip;
}
function uploadChip(chip){
  if(!(invoke && config.app_id && config.app_secret)){
    chip.classList.add('err');
    chip.title = '未配置飞书应用凭证，无法换取 image_key';
    return;
  }
  const base64 = (chip._dataUrl || '').split(',')[1];
  if(!base64){ chip.classList.add('err'); chip.title = '无图片数据'; return; }
  invoke('upload_image', { imageBase64: base64, filename: chip.dataset.name, appId: config.app_id, appSecret: config.app_secret })
    .then(key => { chip._imageKey = key; chip.classList.remove('err'); chip.title = 'image_key 已换取'; })
    .catch(e => {
      chip.classList.add('err');
      chip.title = '上传失败：' + (e.message || e);
      log('error', '图片上传失败 ' + chip.dataset.name + ' → ' + (e.message || e));
    });
}
[ed, expEd].forEach(el => {
  el.addEventListener('click', e => {
    const x = e.target.closest('.cx');
    if (!x) return;
    e.preventDefault(); e.stopPropagation();
    const chip = x.closest('.chip');
    if (chip) chip.remove();
    syncEmpty(el); refresh();
  });
});

/* ===================== 消息组装与发送（组装/签名/历史全在 Rust core，方案 v3 §九 D7） ===================== */

/* 编辑器内容收集 */
function collectEd(el){
  const text = edText(el).replace(/\u200b/g,'').replace(/\n{3,}/g,'\n\n').trim();
  const chips = $$('.chip', el);
  const imgKeys = chips.filter(c => c._imageKey).map(c => c._imageKey);
  const pending = chips.filter(c => !c._imageKey && !c.classList.contains('err')).length;
  return {text, chips, imgKeys, pending};
}

/* 生成缩略图 dataURL（用于历史记录展示，避免完整图片撑大本地配置）。
 * 边长取 256：历史里单图格子最宽 340px，96px 被放大后明显发虚（M3 后历史图片回显看不清） */
const THUMB_MAX = 256;
function makeThumb(dataUrl){
  return new Promise(res => {
    if(!dataUrl){ res(''); return; }
    const img = new Image();
    img.onload = () => {
      try{
        const s = Math.min(1, THUMB_MAX / Math.max(img.width, img.height));
        const c = document.createElement('canvas');
        c.width = Math.max(1, Math.round(img.width * s));
        c.height = Math.max(1, Math.round(img.height * s));
        const ctx = c.getContext('2d');
        ctx.fillStyle = '#fff';
        ctx.fillRect(0, 0, c.width, c.height);
        ctx.drawImage(img, 0, 0, c.width, c.height);
        res(c.toDataURL('image/jpeg', 0.7));
      }catch(e){ res(''); }
    };
    img.onerror = () => res('');
    img.src = dataUrl;
  });
}

async function send(){
  const el = activeEd();
  if (isBlank(el)) return;
  const bot = config.webhooks[config.last_webhook];
  if(!bot){ toast('未配置机器人', '请先在配置中添加', 'err'); return; }

  const {text, chips, imgKeys, pending} = collectEd(el);
  if(pending){ toast('图片仍在上传中，请稍候', '', 'err'); return; }
  const ft = forcedType();
  const type = ft === 'auto' ? lastAnalysis.t : ft;
  if(type !== 'image' && !text.trim() && !chips.length){ toast('内容为空', '', 'err'); return; }

  const title = el === expEd ? ($('#expandTitle').value.trim() || '') : '';

  // 记录用的图片元数据（与 imgKeys 同序）
  const keyed = chips.filter(c => c._imageKey);
  const thumbs = keyed.length ? await Promise.all(keyed.map(c => makeThumb(c._dataUrl))) : [];
  const media = keyed.length ? {
    image_names: keyed.map(c => c.dataset.name),
    img_keys: keyed.map(c => c._imageKey),
    thumbs
  } : {};
  const extra = {};
  if (type === 'text' || type === 'post') extra.text = text;
  if (Object.keys(media).length) extra.media = media;
  const summary = type === 'image'
    ? chips.map(c=>c.dataset.name).join(', ') + ' (' + chips.length + ' 张图)'
    : null;

  const t0 = performance.now();
  log('info', 'send: 开始发送 type=' + type + ' 机器人=' + (bot.name || '?'));
  try{
    if(!invoke) throw 'Tauri 环境不可用';
    // 组装 / 签名 / 发送 / 历史写入全部在 core（D7）；网络失败也会记录 failed/unknown
    const r = await invoke('send_message', {
      botKey: null, msgType: ft, text, imageKeys: imgKeys,
      title: title || null, summary: summary || null,
      extra: Object.keys(extra).length ? extra : null,
    });
    const dt = Math.round(performance.now() - t0);
    log(r.ok ? 'info' : 'error', 'send: 完成 type=' + type + ' ' + r.status_line + ' · ' + dt + 'ms');
    addRecord(r.record, false);   // 历史已由 core 落盘，这里只做界面渲染
    el.innerHTML = '';
    syncEmpty(el); refresh();
    if(el === expEd) $('#expandTitle').value = '';
    toast(r.ok ? ('已发送到 ' + bot.name) : '发送失败', r.status_line, r.ok ? '' : 'err');
  }catch(e){
    const msg = typeof e === 'string' ? e : (e && e.message) || String(e);
    log('error', 'send: 发送失败 ' + msg);
    // Err 仅发生在发送之前（bot 缺失/组装失败等），core 未记录历史
    toast('发送失败', msg, 'err');
  }
}

/* ===================== 消息流渲染 ===================== */
const BIRD = '<svg viewBox="0 0 22 28" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 5.5c-1.2.5-2.4.9-3.7 1a5.4 5.4 0 0 0-9.4 4.8C4.9 11.1 2 9.4 0 6.7c-1 3 .1 6.4 2.8 8.2-.9 0-1.8-.3-2.6-.7 0 3 2.1 5.5 5 6.1-.9.2-1.8.3-2.6.1.7 2.4 3 4.2 5.7 4.3A11 11 0 0 1 0 27.5"/></svg>';
const OK_ICO = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6L9 17l-5-5"/></svg>';
const WARN_ICO = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M12 8v5"/><path d="M12 16.5v.01"/></svg>';
const FILE_ICO = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/><path d="M8 13h8"/><path d="M8 17h5"/></svg>';
const X_ICO = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M18 6L6 18M6 6l12 12"/></svg>';
const COPY_ICO = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15V5a2 2 0 0 1 2-2h10"/></svg>';
const RESEND_ICO = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M23 4v6h-6"/><path d="M20.5 15a9 9 0 1 1-2.1-9.4L23 10"/></svg>';

function inlineHtml(s){
  let out = esc(s);
  out = out.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  out = out.replace(/`([^`]+)`/g, '<code>$1</code>');
  out = out.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="#" data-href="$2">$1</a>');
  out = out.replace(/@(所有人|[^\s@<]{1,12})/g, '<span class="at">@$1</span>');
  return out;
}
function renderTextHtml(raw){
  const lines = String(raw).split('\n'); let html = '', inUl = false;
  const closeUl = () => { if (inUl){ html += '</ul>'; inUl = false; } };
  for (const line of lines){
    const t = line.trim();
    if (/^[-*]\s+/.test(t)){
      if (!inUl){ html += '<ul>'; inUl = true; }
      html += '<li>' + inlineHtml(t.replace(/^[-*]\s+/,'')) + '</li>';
      continue;
    }
    closeUl();
    if (!t) continue;
    if (/^#{1,3}\s+/.test(t)){ html += '<h4>' + inlineHtml(t.replace(/^#{1,3}\s+/,'')) + '</h4>'; continue; }
    if (/^>\s?/.test(t)){ html += '<div class="quote">' + inlineHtml(t.replace(/^>\s?/,'')) + '</div>'; continue; }
    html += '<p>' + inlineHtml(t) + '</p>';
  }
  closeUl();
  return html || '<p>' + inlineHtml(raw) + '</p>';
}
function msgActs(kinds){
  const icons = { copy: COPY_ICO, resend: RESEND_ICO, remove: X_ICO };
  const titles = { copy:'复制内容', resend:'重新发送', remove:'删除这条记录' };
  return '<div class="msg-acts">' + kinds.map(k =>
    '<button data-act="' + k + '"' + (k === 'remove' ? ' class="danger"' : '') +
    ' title="' + titles[k] + '" aria-label="' + titles[k] + '">' + icons[k] + '</button>').join('') + '</div>';
}
function whoLine(rec){
  if (rec.dir === 'in'){
    return '<div class="who"><b>取回文件</b><span class="dir in">接收</span><span class="time">' + fmtClock(new Date(rec.time)) + '</span></div>';
  }
  const kindTag = { text:'文本', post:'富文本', image:'图片', interactive:'交互卡片' }[rec.kind] || '';
  return '<div class="who"><b>青鸟</b><span class="tag">' + esc(kindTag) + '</span>' +
         '<span class="dir">发出</span><span class="time">' + fmtClock(new Date(rec.time)) + '</span></div>';
}
function botAv(){ return '<div class="av bot" aria-hidden="true">' + BIRD + '</div>'; }
function personAv(){ return '<div class="av person" aria-hidden="true">取</div>'; }

/* 图片元数据定位：M3（前端改调 Rust core）起媒体信息由 extra.media 嵌套落盘，
 * 而 v0.4 及更早的记录是顶层 image_names/img_keys/thumbs —— 两种都要认 */
function recMedia(rec){
  const m = rec && rec.media;
  if (m && typeof m === 'object' &&
      (Array.isArray(m.image_names) || Array.isArray(m.img_keys) || Array.isArray(m.thumbs))){
    return m;
  }
  return rec || {};
}
function bubbleHtml(rec){
  const kind = rec.kind;
  if (kind === 'text'){
    return '<div class="bubble plain">' + renderTextHtml(rec.text || rec.summary) + '</div>';
  }
  if (kind === 'image'){
    const media = recMedia(rec);
    const names = Array.isArray(media.image_names) ? media.image_names.slice() : [];
    const thumbs = Array.isArray(media.thumbs) ? media.thumbs : [];
    const keys = Array.isArray(media.img_keys) ? media.img_keys : [];
    let n = Math.max(names.length, thumbs.length, keys.length);
    // CLI 发送 / 更早的记录没有媒体元数据，但 payload 里一定有 image_key
    if (!n && rec.payload && rec.payload.content && rec.payload.content.image_key) n = 1;
    if (!n) return '<div class="bubble plain">' + renderTextHtml(rec.summary || '(空)') + '</div>';
    while (names.length < n) names.push('');
    const tiles = names.map((n2,i) => thumbs[i]
      ? '<div class="thumb ' + TONES[i%3] + ' timg" style="background-image:url(' + thumbs[i] + ')"></div>'
      : '<div class="thumb ' + TONES[i%3] + '"><span class="tn">' + esc(n2 || '图片') + '</span></div>').join('');
    return '<div class="bubble"><div class="bub-head img">' + n + ' 张图片<span class="htype">图片 · image</span></div>' +
      '<div class="img-grid' + (n === 1 ? ' one' : '') + '">' + tiles + '</div>' +
      '<div class="bub-foot"><span class="ok">' + OK_ICO + '已发送 · ' + n + ' 张图</span><span>image_key 已换取</span></div></div>';
  }
  if (kind === 'interactive'){
    return '<div class="bubble"><div class="bub-head card">交互卡片<span class="htype">interactive</span></div>' +
      '<div class="card-msg"><div class="cm-desc">' + esc(rec.summary) + '</div></div>' +
      '<div class="bub-foot"><span class="ok">' + OK_ICO + '已发送</span><span>' + esc(rec.status||'') + '</span></div></div>';
  }
  // post
  let body = '<p>' + esc(rec.summary) + '</p>';
  if (rec.payload && rec.payload.content && rec.payload.content.post){
    try{
      const post = rec.payload.content.post.zh_cn;
      // image_key → 缩略图映射（附件图片按 imgKeys 顺序存于 media，见 recMedia）
      const media = recMedia(rec);
      const keyThumb = {};
      if (Array.isArray(media.img_keys) && Array.isArray(media.thumbs)){
        media.img_keys.forEach((k, i) => { if (k && media.thumbs[i]) keyThumb[k] = media.thumbs[i]; });
      }
      const lines = [];
      for (const row of (post.content||[])){
        let line = '';
        for (const el of row){
          if (el.tag === 'text') line += esc(el.text||'');
          else if (el.tag === 'a') line += '<a href="#" onclick="return false">' + esc(el.text||el.href||'') + '</a>';
          else if (el.tag === 'at') line += '<span class="at">@' + esc(el.user_name||'某人') + '</span>';
          else if (el.tag === 'img'){
            const t = keyThumb[el.image_key];
            line += t ? '<img class="post-img" src="' + t + '" alt="图片">' : '<span class="post-img-ph">图片</span>';
          }
        }
        lines.push(line);
      }
      body = lines.map(l => l ? '<p>' + l + '</p>' : '').join('') || body;
      if (post.title) body = '<h4>' + esc(post.title) + '</h4>' + body;
    }catch(e){}
  }
  return '<div class="bubble"><div class="bub-head post">' + esc(rec.summary.slice(0,24) || '富文本') + '<span class="htype">富文本 · post</span></div>' +
    '<div class="bub-body">' + body + '</div>' +
    '<div class="bub-foot"><span class="' + (rec.ok ? 'ok' : 'st-err') + '">' + (rec.ok ? OK_ICO + '已发送' : WARN_ICO + '发送失败') + '</span><span>' + esc(rec.status||'') + '</span></div></div>';
}
function fileCardHtml(rec){
  const dir = rec.dir || 'out';
  const cls = 'filecard' + (dir === 'in' ? ' down' : '') +
    (rec.state === 'done' || rec.state === 'downloaded' ? ' done' : '') +
    (rec.state === 'failed' || rec.state === 'cancelled' ? ' fail' : '');
  const pct = Math.round((rec.bytes_done || 0) * 100 / Math.max(1, rec.size || 1));
  const running = rec.state === 'uploading' || rec.state === 'downloading';
  let st;
  if (rec.state === 'done' || rec.state === 'downloaded'){
    // 「N 分钟」须与 Rust `crypto::FRESHNESS_WINDOW_MINUTES`（链接有效期）一致
    st = '<span class="st">' + OK_ICO + (dir === 'in' ? '已保存到下载目录' : '取回链接已发到 ' + esc(rec.bot_name || '群') + ' · 10 分钟内有效') + '</span>' +
         '<span class="bacts">' + (dir === 'in' ? '<button data-open-dir>打开目录</button>' : '<button class="hi" data-copy-link>复制链接</button>') + '</span>';
  } else if (rec.state === 'failed' || rec.state === 'cancelled'){
    st = '<span class="st">' + WARN_ICO + esc(rec.error || (rec.state === 'cancelled' ? '已取消' : '传输失败')) + '</span>' +
         '<span class="bacts"><button data-retry>重试</button></span>';
  } else {
    st = '<span class="st" data-st>' + (dir === 'in' ? '正在下载，完成后保存到「下载」目录' : '端到端加密传输中') + '</span>' +
         '<span class="bacts"><button data-cancel>取消</button></span>';
  }
  return '<div class="' + cls + '" data-rec-fp="' + esc(rec.fingerprint || '') + '">' +
    '<div class="fc-main">' +
      '<span class="fc-ico' + (dir === 'in' ? ' down' : '') + (rec.state === 'failed' || rec.state === 'cancelled' ? ' fail' : '') + '" aria-hidden="true">' + FILE_ICO + '</span>' +
      '<div class="fc-info"><b title="' + esc(rec.name) + '">' + esc(rec.name) + '</b>' +
        '<div class="fmeta"><span>' + fmtSize(rec.size) + '</span>' +
        (running ? '<span class="live' + (dir === 'in' ? ' down' : '') + '" data-rate>' + (dir === 'in' ? '正在连接取回链接…' : '正在上传…') + '</span><span data-pct>' + pct + '%</span>' : '') +
        '</div></div>' +
      (running ? '<button class="fc-cancel" data-cancel title="取消传输" aria-label="取消传输">' + X_ICO + '</button>' : '') +
    '</div>' +
    '<div class="fc-bar" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="' + pct + '"><i style="width:' + pct + '%"></i></div>' +
    '<div class="fc-bot">' + st + '</div>' +
  '</div>';
}
function renderRecord(rec){
  const art = document.createElement('article');
  art.className = 'msg';
  art.dataset.dir = rec.dir === 'in' ? 'in' : 'out';
  let inner;
  if (rec.kind === 'file'){
    inner = (rec.dir === 'in' ? personAv() : botAv()) +
      '<div class="col"><div class="stack">' + whoLine(rec) + fileCardHtml(rec) + '</div>' +
      msgActs(['remove']) + '</div>';
  } else {
    const acts = ['copy', 'resend', 'remove'].filter(k => k !== 'resend' || rec.dir === 'out');
    inner = botAv() + '<div class="col"><div class="stack">' + whoLine(rec) + bubbleHtml(rec) + '</div>' +
      msgActs(acts) + '</div>';
  }
  art.innerHTML = inner;
  art._rec = rec;
  return art;
}
function renderStream(){
  const stream = $('#stream');
  stream.innerHTML = '';
  let lastDay = null;
  for (const rec of config.history){
    const dk = dayKey(rec.time);
    if (dk !== lastDay){
      lastDay = dk;
      const sep = document.createElement('div');
      sep.className = 'daysep';
      sep.textContent = dayLabel(rec.time);
      stream.appendChild(sep);
    }
    stream.appendChild(renderRecord(rec));
  }
  refreshStats();
  scrollToEnd(false);
}
function addRecord(rec, persist = true){
  config.history.push(rec);
  // 100 条上限由 core 的 HISTORY_CAP 在落盘时强制（M0b），前端不再 shift
  // persist=false：记录已由 Rust core 落盘（消息发送，D7），此处只渲染
  if (persist) {
    if (invoke) invoke('append_history_item', { rec }).catch(e => console.error('历史追加失败', e));
    else saveConfig(); // 非 Tauri 环境（原型预览）兜底
  }
  // 增量插入
  const stream = $('#stream');
  const lastRec = config.history[config.history.length - 2];
  if (!lastRec || dayKey(lastRec.time) !== dayKey(rec.time)){
    const sep = document.createElement('div');
    sep.className = 'daysep';
    sep.textContent = dayLabel(rec.time);
    stream.appendChild(sep);
  }
  const art = renderRecord(rec);
  stream.appendChild(art);
  refreshStats();
  scrollToEnd();
  return art;
}
function scrollToEnd(smooth = true){
  const sc = $('#chatScroll');
  requestAnimationFrame(() => { sc.scrollTo({top: sc.scrollHeight, behavior: smooth ? 'smooth' : 'auto'}); });
}
function refreshStats(){
  const all = config.history;
  const out = all.filter(m => (m.dir||'out') === 'out').length;
  const inc = all.filter(m => m.dir === 'in').length;
  $('#statUp').textContent = '发出 ' + out;
  $('#statDown').textContent = '接收 ' + inc;
  $('#chatEmpty').classList.toggle('show', all.length === 0);
}

/* ===================== 消息悬停操作 ===================== */
$('#stream').addEventListener('click', async e => {
  const actBtn = e.target.closest('.msg-acts button[data-act]');
  if (actBtn){
    const art = actBtn.closest('.msg');
    const rec = art._rec;
    const act = actBtn.dataset.act;
    if (act === 'remove'){
      // R8/M0b：删除走独立 IPC（锁内按 time+kind 定位），本地仅同步渲染
      if (invoke && rec.time && rec.kind) {
        invoke('delete_history_item', { time: rec.time, kind: rec.kind })
          .catch(e => toast('删除失败: ' + e, '', 'err'));
      }
      const idx = config.history.indexOf(rec);
      if (idx >= 0) config.history.splice(idx, 1);
      art.remove();
      // 重绘日期分隔
      renderStream();
      toast('已删除这条记录');
    } else if (act === 'copy'){
      const body = $('.bub-body, .bubble.plain', art);
      if (body && navigator.clipboard) navigator.clipboard.writeText(body.innerText).catch(() => {});
      toast('已复制到剪贴板');
    } else if (act === 'resend'){
      if (rec.payload){
        try{
          if(!invoke) throw 'Tauri 环境不可用';
          // 重发走 core：重签名 + 按 time 更新原历史条目（不追加）
          const r = await invoke('resend_payload', {payload: rec.payload, botKey: null, recTime: rec.time});
          rec.ok = r.ok; rec.status = r.status_line; rec.state = r.state;
          saveConfig(); renderStream();
          toast(r.ok ? '已重新发送' : '重新发送失败', r.status_line, r.ok ? '' : 'err');
        }catch(e2){ toast('重新发送失败', String(e2 && e2.message || e2), 'err'); }
      }
    }
    return;
  }
  // filecard 内按钮
  const copyBtn = e.target.closest('[data-copy-link]');
  if (copyBtn){
    const rec = copyBtn.closest('.msg')?._rec;
    const link = rec && rec.link;
    if (link && navigator.clipboard) navigator.clipboard.writeText(link).catch(() => {});
    copyBtn.textContent = '已复制';
    setTimeout(() => { copyBtn.textContent = '复制链接'; }, 1600);
    toast('取回链接已复制');
    return;
  }
  if (e.target.closest('[data-open-dir]')){
    if (invoke){ invoke('open_download_dir').catch(err => toast('打开目录失败', String(err), 'err')); }
    return;
  }
  const retryBtn = e.target.closest('[data-retry]');
  if (retryBtn){ pickFile(); return; }
  const cancelBtn = e.target.closest('[data-cancel]');
  if (cancelBtn){
    const art = cancelBtn.closest('.msg');
    const rec = art && art._rec;
    if (rec && rec.task_id && invoke){
      invoke('transfer_cancel', {taskId: rec.task_id}).catch(() => {});
    }
    return;
  }
});

/* ===================== 清空记录 ===================== */
const clearBtn = $('#clearBtn');
let clearTimer = null;
clearBtn.addEventListener('click', () => {
  if (!clearBtn.classList.contains('confirming')){
    clearBtn.classList.add('confirming');
    clearBtn.innerHTML = clearBtn.innerHTML.replace('清空记录', '再点一次确认');
    clearTimer = setTimeout(resetClear, 4000);
    return;
  }
  resetClear();
  config.history = [];
  saveConfig();
  renderStream();
  toast('消息记录已清空');
});
function resetClear(){
  clearTimeout(clearTimer);
  clearBtn.classList.remove('confirming');
  clearBtn.innerHTML = clearBtn.innerHTML.replace('再点一次确认', '清空记录');
}

/* ===================== 文件传输：选择 / 确认 / 上传 ===================== */
function pickFile(){
  closeAddMenu();
  if (invoke){
    invoke('plugin:dialog|open', {options: {multiple: false, title: '选择要发送的文件'}})
      .then(path => { if (path) handleFile({path, name: String(path).split('/').pop().split('\\').pop(), size: 0}); })
      .catch(e => {
        log('warn', '文件选择器不可用: ' + (e.message || e));
        toast('文件选择器不可用', '请使用拖拽方式添加文件', 'err');
      });
  } else {
    toast('浏览器降级模式', '文件传输需要 Tauri 环境', 'err');
  }
}
function handleFile(file){
  pendingFile = file;
  $('#confirmTarget').textContent = (config.webhooks[config.last_webhook] || {}).name || '-';
  $('#confirmName').textContent = file.name;
  $('#confirmName').title = file.name;
  const big = file.size > 100 * 1048576;
  $('#confirmMeta').textContent = (file.size ? fmtSize(file.size) + ' · ' : '') + (big ? '超出单文件上限 100 MB' : '端到端加密 · 群里只出现一条取回链接');
  $('#confirmIconShape').innerHTML = big
    ? '<circle cx="12" cy="12" r="9"/><path d="M12 8v5"/><path d="M12 16.5v.01"/>'
    : '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/><path d="M8 13h8"/><path d="M8 17h5"/>';
  $('#confirmSend').disabled = big;
  $('#confirmSend').style.opacity = big ? '0.5' : '';
  $('#confirmSend').textContent = big ? '无法发送' : '发送';
  $('#confirmOverlay').classList.add('show');
}
function closeConfirm(){ $('#confirmOverlay').classList.remove('show'); }
$('#confirmSend').addEventListener('click', async () => {
  if ($('#confirmSend').disabled) return;
  const f = pendingFile;
  closeConfirm();
  if (!f || !invoke){ toast('Tauri 环境不可用', '', 'err'); return; }
  const rec = {
    time: nowIso(), kind: 'file', dir: 'out',
    name: f.name, size: f.size || 0,
    state: 'uploading', bytes_done: 0,
    bot_name: (config.webhooks[config.last_webhook] || {}).name
  };
  const art = addRecord(rec);
  const card = $('.filecard', art);
  const bot = config.webhooks[config.last_webhook];
  try{
    const res = await invoke('transfer_start_upload', {
      path: f.path,
      webhookUrl: bot ? bot.url : '',
      webhookSecret: bot && bot.secret ? bot.secret : ''
    });
    rec.task_id = res.task_id;
    if (res.size) rec.size = res.size;
    liveTasks.set(res.task_id, {record: rec, el: card});
    saveConfig();
    // 补偿：终态事件可能早于本次注册已发出（小文件上传极快）
    setTimeout(() => pollFinalState(res.task_id), 300);
  }catch(e){
    rec.state = 'failed';
    rec.error = String(e && e.message || e).slice(0, 120);
    saveConfig(); renderStream();
    toast('无法开始传输', rec.error, 'err');
  }
});
function findCardEl(rec){
  for (const art of $$('#stream .msg')){
    if (art._rec === rec) return $('.filecard', art);
  }
  return null;
}
$('#confirmCancel').addEventListener('click', closeConfirm);
$('#confirmClose').addEventListener('click', closeConfirm);
$('#confirmOverlay').addEventListener('click', e => { if (e.target === e.currentTarget) closeConfirm(); });

/* 传输进度事件（监听 + 注册后轮询补偿，防终态事件早于注册到达） */
function applyProgress(p){
  const t = liveTasks.get(p.task_id);
  if (!t) return;
  const {record: rec} = t;
  if (p.bytes_done !== undefined) rec.bytes_done = p.bytes_done;
  if (p.bytes_total) rec.size = p.bytes_total;
  if (p.state === 'running'){
    const pct = Math.round((rec.bytes_done||0) * 100 / Math.max(1, rec.size||1));
    const card = findCardEl(rec);
    if (card){
      $('.fc-bar', card).setAttribute('aria-valuenow', String(pct));
      $('.fc-bar i', card).style.width = pct + '%';
      const rate = $('[data-rate]', card), pctEl = $('[data-pct]', card);
      if (pctEl) pctEl.textContent = pct + '%';
      if (rate && p.rate_bps) rate.textContent = (rec.dir === 'in' ? '下载中 ' : '正在上传 ') + (p.rate_bps/1048576).toFixed(1) + ' MB/s';
    }
  } else if (p.state === 'done'){
    rec.state = rec.dir === 'in' ? 'downloaded' : 'done';
    rec.bytes_done = rec.size;
    if (p.link) rec.link = p.link;
    if (p.final_path) rec.final_path = p.final_path;
    liveTasks.delete(p.task_id);
    saveConfig(); renderStream();
    if (rec.dir === 'out') toast('文件已发出', '取回链接已发到 ' + (rec.bot_name || '群'));
    else toast('文件已保存', rec.name);
  } else if (p.state === 'failed' || p.state === 'cancelled'){
    rec.state = p.state;
    rec.error = (p.error || (p.state === 'cancelled' ? '已取消' : '传输失败')).slice(0, 120);
    liveTasks.delete(p.task_id);
    saveConfig(); renderStream();
    if (p.state === 'failed') toast('传输失败', rec.error, 'err');
  }
  // 取回弹窗开出去的那次下载收尾了 → 地址栏重新可用（一次只跑一个取回）
  if (p.state !== 'running' && rvLockedRec === rec){ rvLockedRec = null; rvLock(false); }
}
listenEvent('transfer://progress', ev => applyProgress(ev.payload || {}));
/* 浏览器取回（点聊天里的链接）完成 → 补写历史记录；应用内取回已有记录，去重跳过 */
listenEvent('transfer://downloaded', ev => {
  const p = ev.payload || {};
  if (liveTasks.has(p.task_id)) return;
  if (p.fingerprint && config.history.some(r => r.fingerprint === p.fingerprint && ['downloading','downloaded'].includes(r.state))) return;
  addRecord({
    time: nowIso(), kind: 'file', dir: 'in',
    name: p.name || '', size: p.size || 0,
    state: 'downloaded', bytes_done: p.size || 0,
    final_path: p.final_path || '',
    fingerprint: p.fingerprint || ''
  });
  toast('文件已保存', p.name || '');
});
async function pollFinalState(taskId){
  if (!invoke) return;
  try{
    const p = await invoke('transfer_task_state', {taskId});
    if (p) applyProgress(p);
  }catch(e){}
}

/* ===================== 取回文件：浏览器式弹窗 =====================
   视口里渲染的取回页与 assets/transfer-confirm.html 同一套排版；
   下载只跑一次：聊天记录是任务的宿主，取回页只是它的显示器。 */
const retrieveBtn = $('#retrieveBtn'), rvOverlay = $('#retrieveOverlay');
const rvInput = $('#rvInput'), rvViewport = $('#rvViewport');
const rvScreens = $$('.rv-screen', rvOverlay), rvStages = $$('.rv-stage', rvOverlay);
let rvSession = null;        // transfer_parse_payload 返回的会话信息
let rvTtlTimer = null, rvTtlLeft = 0;
let rvLockedRec = null;      // 下载期间锁住地址栏的那条记录

function rvScreen(name){
  rvScreens.forEach(s => s.classList.toggle('on', s.dataset.screen === name));
}
function rvStage(name){
  rvStages.forEach(s => s.classList.toggle('on', s.dataset.stage === name));
  rvViewport.scrollTop = 0;
}
function rvLock(on){
  rvInput.disabled = on;
  $('#rvGo').disabled = on;
}
/* 地址栏展示用：本机服务的实际绑定端口（读不到就退回配置端口） */
function rvHost(){
  const m = /^127\.0\.0\.1:(\d+)$/.exec(($('#portBound').textContent || '').trim());
  const port = m ? m[1] : ((config.transfer && config.transfer.configured_port) || 9876);
  return '127.0.0.1:' + port;
}
function rvSentLabel(unix){
  const d = new Date((Number(unix) || 0) * 1000);
  if (isNaN(d.getTime()) || !unix) return '';
  const day = d.toDateString() === new Date().toDateString()
    ? '今天' : (d.getMonth() + 1) + '月' + d.getDate() + '日';
  return day + ' ' + fmtClock(d);
}
function rvPaintTtl(){
  const m = Math.floor(rvTtlLeft / 60), s = rvTtlLeft % 60;
  $('#rvTtl').textContent = m + ':' + (s < 10 ? '0' : '') + s;
}
function rvRunTtl(){
  clearInterval(rvTtlTimer);
  rvPaintTtl();
  rvTtlTimer = setInterval(() => {
    rvTtlLeft--;
    rvPaintTtl();
    if (rvTtlLeft <= 0){
      clearInterval(rvTtlTimer);
      rvFail('链接已经过期', '这条链接超出了 10 分钟的新鲜度窗口。请让对方在青鸟里重新发一次这个文件。');
    }
  }, 1000);
}
/* 打不开就给一张错误页（像浏览器），而不是弹一条提示 */
const RV_SHAPE_HINT = '请把群里那条取回链接整条粘进来，或只粘 t= 后面那段取回码。';
function rvFail(title, msg){
  clearInterval(rvTtlTimer);
  $('#rvErrTitle').textContent = title;
  $('#rvErrMsg').textContent = msg;
  rvScreen('error');
  rvInput.focus();
  rvInput.select();
}
function openRetrieve(){
  rvOverlay.classList.add('show');
  rvInput.focus();
  if (rvInput.value) rvInput.select();
}
function closeRetrieve(){
  rvOverlay.classList.remove('show');
  retrieveBtn.focus();
}
async function rvOpen(){
  const v = (rvInput.value || '').trim();
  if (!v){ rvFail('地址栏是空的', RV_SHAPE_HINT); return; }
  if (!invoke){ rvFail('取回需要青鸟桌面端', '当前环境无法访问本机传输服务。'); return; }
  const go = $('#rvGo');
  if (go.disabled) return;
  go.disabled = true;
  try{
    // 形态校验交给后端（唯一权威）：链接被飞书包装、百分号编码或整条粘贴时也能认出来
    const meta = await invoke('transfer_parse_payload', {input: v});
    const ttl = Math.round((meta.expires_at || 0) - Date.now() / 1000);
    if (ttl <= 0){ rvFail('链接已经过期', '这条链接超出了 10 分钟的新鲜度窗口。请让对方在青鸟里重新发一次这个文件。'); return; }
    rvSession = meta;
    /* 粘的是裸取回码时把地址栏补成完整链接（纯展示，不影响已发给后端的输入） */
    if (!/^https?:\/\//i.test(v) && /^[A-Za-z0-9_-]{24,}$/.test(v)) rvInput.value = 'http://' + rvHost() + '/dl?t=' + v;
    const name = meta.name || '未命名文件';
    $('#rvFileName').textContent = name;
    $('#rvFileName').title = name;
    $('#rvFileSize').textContent = fmtSize(meta.size || 0);
    const ext = /\.([A-Za-z0-9]{1,8})$/.exec(name);
    $('#rvFileExt').textContent = ext ? ext[1].toUpperCase() : '';
    $('#rvSentTime').textContent = rvSentLabel(meta.created_at);
    const dir = meta.download_dir || '';
    $('#rvSaveDir').textContent = dir || '系统下载目录';
    $('#rvSaveDir').title = dir;
    const path = (dir ? dir.replace(/\/+$/, '') + '/' : '') + name;
    $('#rvPath').textContent = path;
    $('#rvPath').title = path;
    rvTtlLeft = ttl;
    rvScreen('page');
    rvStage('pending');
    rvRunTtl();
    $('#rvStart').focus();
  }catch(e){
    rvFail(String(e && e.message || e).slice(0, 160), RV_SHAPE_HINT);
  }finally{
    go.disabled = false;
  }
}
/* 开始下载：取回页不留进度，任务落到消息记录里 */
$('#rvStart').addEventListener('click', async () => {
  const s = rvSession;
  if (!s || !invoke) return;
  clearInterval(rvTtlTimer);
  rvLock(true);
  const rec = {
    time: nowIso(), kind: 'file', dir: 'in',
    name: s.name || '', size: s.size || 0,
    state: 'downloading', bytes_done: 0,
    fingerprint: s.fingerprint || ''
  };
  const art = addRecord(rec);
  const card = $('.filecard', art);
  rvLockedRec = rec;
  rvStage('started');
  closeRetrieve();
  toast('已开始下载', '进度在消息记录里');
  try{
    const res = await invoke('transfer_confirm_download', {sessionId: s.session_id});
    if (res.already_done){
      // 幂等：该文件已取回过（§9.4 已消费缓存）
      rec.state = 'downloaded';
      rec.final_path = res.final_path || '';
      rec.bytes_done = rec.size;
      rvLockedRec = null; rvLock(false);
      saveConfig(); renderStream();
      toast('该文件已取回过', '不会重复下载');
      return;
    }
    rec.task_id = res.task_id;
    liveTasks.set(res.task_id, {record: rec, el: card});
    saveConfig();
    // 补偿：终态事件可能早于本次注册已发出
    setTimeout(() => pollFinalState(res.task_id), 300);
  }catch(e){
    const msg = String(e && e.message || e).slice(0, 120);
    rec.state = 'failed'; rec.error = msg;
    rvLockedRec = null; rvLock(false);
    saveConfig(); renderStream();
    toast('无法开始下载', msg, 'err');
  }
});
retrieveBtn.addEventListener('click', openRetrieve);
$('#rvGo').addEventListener('click', rvOpen);
rvInput.addEventListener('keydown', e => { if (e.key === 'Enter') rvOpen(); });
$('#rvClose').addEventListener('click', closeRetrieve);
$('#rvCloseBtn').addEventListener('click', closeRetrieve);
document.addEventListener('click', () => {
  closeAddMenu();
  toggleBotMenu(false);
});

/* ===================== 添加菜单 / 拖拽 ===================== */
const addMenu = $('#addMenu'), addBtn = $('#btnAdd');
function closeAddMenu(){
  addMenu.classList.remove('show');
  addBtn.setAttribute('aria-expanded', 'false');
}
addBtn.addEventListener('click', e => {
  e.stopPropagation();
  const show = !addMenu.classList.contains('show');
  addMenu.classList.toggle('show', show);
  addBtn.setAttribute('aria-expanded', String(show));
});
addMenu.addEventListener('click', e => e.stopPropagation());
$('#addImage').addEventListener('click', pickImages);
$('#addFile').addEventListener('click', pickFile);

let imgArmed = false;
function pickImages(){
  closeAddMenu();
  imgArmed = true;
  const inp = $('#imgPicker');
  inp.value = '';
  inp.click();
}
$('#imgPicker').addEventListener('change', function(){
  if (!imgArmed) return;
  imgArmed = false;
  const el = activeEd();
  Array.prototype.forEach.call(this.files, f => {
    const rd = new FileReader();
    rd.onload = ev => insertChip(el, f.name, ev.target.result);
    rd.readAsDataURL(f);
  });
});

$('#btnEmoji').addEventListener('click', () => insertNode(activeEd(), document.createTextNode('🙂')));
$('#btnAt').addEventListener('click', () => insertNode(activeEd(), document.createTextNode('@所有人')));
$('#btnSend').addEventListener('click', send);
$('#expandSend').addEventListener('click', send);

/* 拖拽 */
const IMG_MIME = { png:'image/png', jpg:'image/jpeg', jpeg:'image/jpeg', gif:'image/gif', webp:'image/webp', bmp:'image/bmp', svg:'image/svg+xml' };
const dropzone = $('#dropzone');
function showDrop(on){
  dropzone.classList.toggle('show', on);
  dropzone.setAttribute('aria-hidden', String(!on));
}
function handleDroppedPaths(paths){
  const IMG_EXT = /\.(png|jpe?g|gif|webp|bmp|svg)$/i;
  for (const p of (paths || [])){
    const name = String(p).split('/').pop().split('\\').pop();
    if (IMG_EXT.test(name)){
      if (invoke){
        invoke('read_file_base64', {path: p}).then(b64 => {
          // MIME 必须完整（'data:image;base64,…' 缺子类型，浏览器解不出来 → 缩略图空白）
          const mime = IMG_MIME[(name.split('.').pop() || '').toLowerCase()] || 'image/png';
          insertChip(activeEd(), name, 'data:' + mime + ';base64,' + b64);
        }).catch(e => toast('读取图片失败', String(e).slice(0,80), 'err'));
      }
    } else {
      handleFile({path: p, name, size: 0});
      break; // 一次只处理一个文件，与单文件上限口径一致
    }
  }
}
if (TAURI && TAURI.webview && TAURI.webview.getCurrentWebview){
  try{
    TAURI.webview.getCurrentWebview().onDragDropEvent(ev => {
      const p = ev.payload || {};
      if (p.type === 'enter' || p.type === 'over') showDrop(true);
      else if (p.type === 'drop'){ showDrop(false); handleDroppedPaths(p.paths); }
      else showDrop(false);
    });
  }catch(e){ console.error('拖拽初始化失败', e); }
}

/* ===================== 放大编辑 ===================== */
const expand = $('#expand');
function moveChildren(from, to){
  while (from.firstChild) to.appendChild(from.firstChild);
}
function openExpand(){
  // 搬 DOM 节点而不是拷 innerHTML：chip 的 _dataUrl/_imageKey 是 JS 属性，
  // innerHTML 克隆会丢掉，收起后图片既发不出去也会卡在「仍在上传中」
  const hadContent = !isBlank(ed);
  RANGES.delete(ed);   // 搬走后旧 range 偏移可能越界，插图片时走 append 兜底
  moveChildren(ed, expEd);
  syncEmpty(ed); syncEmpty(expEd);
  $('#expandTitle').value = '';
  $('#expandFrom').textContent = hadContent
    ? '已带入输入框里的内容 · 收起时会带回去'
    : '输入框还是空的 · 收起时会带回去';
  expand.classList.add('show');
  refresh();
  setTimeout(() => { expEd.focus(); placeCaretEnd(expEd); }, 60);
}
function closeExpand(){
  RANGES.delete(expEd);
  moveChildren(expEd, ed);
  syncEmpty(expEd); syncEmpty(ed);
  expand.classList.remove('show');
  refresh();
  setTimeout(() => { ed.focus(); placeCaretEnd(ed); }, 40);
}
function placeCaretEnd(el){
  if (isBlank(el)) return;
  const r = document.createRange();
  r.selectNodeContents(el); r.collapse(false);
  const sel = window.getSelection();
  sel.removeAllRanges(); sel.addRange(r);
  RANGES.set(el, r.cloneRange());
}
$('#btnExpand').addEventListener('click', openExpand);
$('#expandClose').addEventListener('click', closeExpand);
expand.addEventListener('click', e => { if (e.target === expand) closeExpand(); });

/* 展开编辑工具栏：以 Markdown 文本方式插入（发送链路吃 markdown） */
function insertMarkdown(wrapBefore, wrapAfter, prefix){
  const el = expEd;
  const sel = window.getSelection();
  const hasSel = sel && sel.rangeCount && el.contains(sel.getRangeAt(0).commonAncestorContainer) && !sel.isCollapsed;
  if (hasSel){
    const r = sel.getRangeAt(0);
    const frag = r.extractContents();
    const tmp = document.createElement('div');
    tmp.appendChild(frag);
    const text = tmp.innerText;
    insertNode(el, document.createTextNode(prefix ? '' : wrapBefore + text + (wrapAfter || '')));
  }
  if (prefix){
    // 行首前缀：在光标处插入换行 + 前缀
    insertNode(el, document.createTextNode('\n' + prefix + ' '));
  } else if (!hasSel){
    insertNode(el, document.createTextNode(wrapBefore + (wrapAfter || '')));
  }
  syncEmpty(el); refresh();
}
$$('.expand-bar .ebtn').forEach(b => {
  b.addEventListener('click', () => {
    expEd.focus();
    const md = b.dataset.md;
    if (md === 'bold') insertMarkdown('**', '**');
    else if (md === 'italic') insertMarkdown('*', '*');
    else if (md === 'strike') insertMarkdown('~~', '~~');
    else if (md === 'ul') insertMarkdown(null, null, '-');
    else if (md === 'ol') insertMarkdown(null, null, '1.');
    else if (md === 'quote') insertMarkdown(null, null, '>');
    else if (md === 'code') insertMarkdown('`', '`');
    else if (md === 'link') insertMarkdown('[', '](https://)');
    syncEmpty(expEd); refresh();
  });
});
$('#expandAddImage').addEventListener('click', pickImages);
$('#expandAddFile').addEventListener('click', pickFile);

/* ===================== 机器人下拉 & 管理 ===================== */
const botPickerBtn = $('#botPickerBtn'), botMenu = $('#botMenu');
function toggleBotMenu(force){
  const show = typeof force === 'boolean' ? force : !botMenu.classList.contains('show');
  botMenu.classList.toggle('show', show);
  botPickerBtn.setAttribute('aria-expanded', String(show));
}
botPickerBtn.addEventListener('click', e => { e.stopPropagation(); toggleBotMenu(); });
botMenu.addEventListener('click', e => e.stopPropagation());

function renderBotMenu(){
  $$('.bot-item', botMenu).forEach(el => el.remove());
  const divider = $('.bot-menu-divider', botMenu);
  config.webhooks.forEach((bot, idx) => {
    const btn = document.createElement('button');
    btn.className = 'bot-item' + (idx === config.last_webhook ? ' active' : '');
    btn.setAttribute('role','menuitemradio');
    btn.setAttribute('aria-checked', String(idx === config.last_webhook));
    const shortUrl = (bot.url||'').replace('https://open.feishu.cn/open-apis/bot/v2/hook/','open.feishu.cn/…/');
    btn.innerHTML = '<span class="bdot'+(bot.secret?'':' muted')+'" aria-hidden="true"></span>'
      + '<div class="binfo"><b>'+esc(bot.name)+'</b><span>'+esc(shortUrl)+'</span></div>'
      + '<svg class="check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6L9 17l-5-5"/></svg>';
    btn.addEventListener('click', () => {
      config.last_webhook = idx; saveConfig();
      renderBotMenu(); updateBotPicker(); toggleBotMenu(false);
    });
    botMenu.insertBefore(btn, divider);
  });
  const title = $('.bot-menu-title', botMenu);
  if (title) title.textContent = '发送目标 · ' + config.webhooks.length + ' 个机器人';
}
function updateBotPicker(){
  const bot = config.webhooks[config.last_webhook];
  $('#currentBotName').textContent = bot ? bot.name : '未设置';
  $('#currentBotHint').textContent = bot ? (bot.secret ? '已设置签名' : '未设置签名') : '';
  updateBotLabels();
}
function renderBotList(){
  const list = $('#botList');
  list.innerHTML = '';
  config.webhooks.forEach((bot, idx) => {
    const item = document.createElement('div');
    item.className = 'bot-list-item';
    item.dataset.idx = idx;
    const shortUrl = (bot.url||'').replace('https://open.feishu.cn/open-apis/bot/v2/hook/','open.feishu.cn/…/');
    item.innerHTML =
      '<div class="bot-list-head">'
        + '<div class="avt">'+esc((bot.name||'?').slice(0,2))+'</div>'
        + '<div class="meta"><b>'+esc(bot.name)+'</b>'
        + '<div class="row"><span>'+esc(shortUrl)+'</span>'
        + '<span class="tag '+(bot.secret?'ok':'warn')+'">'+(bot.secret?'已签名':'未签名')+'</span></div></div>'
        + '<div class="acts"><button data-action="edit">编辑</button>'
        + '<button class="danger" data-action="delete">删除</button></div></div>'
      + '<div class="bot-list-edit"><div class="edit-grid">'
        + '<input type="text" value="'+esc(bot.name)+'" placeholder="名称" />'
        + '<input type="text" value="'+esc(bot.secret||'')+'" placeholder="签名密钥（留空则不带签名）" />'
        + '<input type="text" class="full" value="'+esc(bot.url||'')+'" placeholder="Webhook 地址" />'
        + '</div><div class="edit-actions">'
        + '<span class="edit-hint">修改会立即生效，不影响已发送的历史消息</span>'
        + '<button data-action="cancel-edit">取消</button>'
        + '<button class="primary" data-action="save-edit">保存修改</button></div></div>'
      + '<div class="bot-list-confirm">'
        + '<span class="msg">确认删除 <b>'+esc(bot.name)+'</b>？此机器人将从青鸟移除，飞书群内的 Webhook 本身不受影响。</span>'
        + '<button data-action="cancel-delete">取消</button>'
        + '<button class="danger" data-action="confirm-delete">确认删除</button></div>';
    list.appendChild(item);
  });
  refreshBotCount();
}
function refreshBotCount(){
  $('#botCountLabel').textContent = '共 ' + config.webhooks.length + ' 个机器人';
  const title = $('#botMenu .bot-menu-title');
  if (title) title.textContent = '发送目标 · ' + config.webhooks.length + ' 个机器人';
}
$('#botList').addEventListener('click', e => {
  const btn = e.target.closest('[data-action]');
  if(!btn) return;
  const item = btn.closest('.bot-list-item');
  if(!item) return;
  const idx = parseInt(item.dataset.idx);
  const action = btn.dataset.action;
  if(action === 'edit'){
    $$('.bot-list-item').forEach(x => { if(x !== item) x.classList.remove('editing','confirming-delete'); });
    item.classList.remove('confirming-delete'); item.classList.add('editing');
    setTimeout(() => item.querySelector('.bot-list-edit input')?.focus(), 0);
  } else if(action === 'save-edit'){
    const inputs = item.querySelectorAll('.bot-list-edit .edit-grid input');
    config.webhooks[idx] = {name: inputs[0].value.trim() || '未命名', secret: inputs[1].value.trim(), url: inputs[2].value.trim()};
    saveConfig(); renderBotList(); renderBotMenu(); updateBotPicker();
  } else if(action === 'cancel-edit'){ item.classList.remove('editing'); }
  else if(action === 'delete'){
    $$('.bot-list-item').forEach(x => { if(x !== item) x.classList.remove('editing','confirming-delete'); });
    item.classList.remove('editing'); item.classList.add('confirming-delete');
  } else if(action === 'cancel-delete'){ item.classList.remove('confirming-delete'); }
  else if(action === 'confirm-delete'){
    config.webhooks.splice(idx, 1);
    if(config.last_webhook >= config.webhooks.length) config.last_webhook = Math.max(0, config.webhooks.length-1);
    saveConfig(); renderBotList(); renderBotMenu(); updateBotPicker();
  }
});
$('#addBotBtn').addEventListener('click', () => {
  const name = $('#addBotName').value.trim(), secret = $('#addBotSecret').value.trim(), url = $('#addBotUrl').value.trim();
  const errEl = $('#addBotError');
  if(!name || !url){
    errEl.textContent = '请填写名称和 Webhook 地址';
    errEl.style.color = 'var(--danger)';
    setTimeout(() => { errEl.textContent = '签名密钥用于自动计算 sign，留空则不带签名'; errEl.style.color = ''; }, 3000);
    return;
  }
  config.webhooks.push({name, secret, url});
  saveConfig(); renderBotList(); renderBotMenu(); updateBotPicker();
  $('#addBotName').value = ''; $('#addBotSecret').value = ''; $('#addBotUrl').value = '';
});

/* ===================== 弹窗通用 ===================== */
const configOverlay = $('#configOverlay');
const manageOverlay = $('#manageOverlay');
$$('.overlay').forEach(o => o.addEventListener('click', e => { if (e.target === o) o.classList.remove('show'); }));

$('#openConfig').addEventListener('click', () => { loadConfigForm(); configOverlay.classList.add('show'); });
$('#closeConfig').addEventListener('click', () => configOverlay.classList.remove('show'));
$('#cancelConfig').addEventListener('click', () => configOverlay.classList.remove('show'));
$('#saveConfig').addEventListener('click', async () => { await saveConfigForm(); configOverlay.classList.remove('show'); });

$('#menuGoManage').addEventListener('click', () => { toggleBotMenu(false); renderBotList(); manageOverlay.classList.add('show'); });
$('#closeManage').addEventListener('click', () => manageOverlay.classList.remove('show'));
$('#closeManageFooter').addEventListener('click', () => manageOverlay.classList.remove('show'));

const tabToPane = {app:'paneApp', agent:'paneAgent', transfer:'paneTransfer', pref:'panePref', about:'paneAbout'};
$$('#configOverlay .nav button').forEach(b => {
  b.addEventListener('click', () => {
    $$('#configOverlay .nav button').forEach(x => x.classList.remove('active'));
    b.classList.add('active');
    const target = tabToPane[b.dataset.tab];
    $$('#configOverlay .pane').forEach(p => p.classList.toggle('active', p.id === target));
    if (target === 'paneAgent') refreshAgentStatus();
  });
});

/* ===================== Agent tab（CLI / 技能安装，模仿 paseo，方案 v3 §七） ===================== */
function agentBadge(el, text, kind){
  el.textContent = text;
  el.style.background = kind === 'ok' ? 'rgba(52,199,89,.15)' : kind === 'warn' ? 'rgba(255,159,10,.18)' : 'var(--track)';
  el.style.color = kind === 'ok' ? '#34c759' : kind === 'warn' ? '#ff9f0a' : 'var(--muted)';
}
async function refreshAgentStatus(){
  const cliBadge = $('#cliStatusBadge'), cliBtn = $('#cliInstallBtn'), cliHint = $('#cliHint');
  const skBadge = $('#skillStatusBadge'), skBtn = $('#skillInstallBtn'), skUn = $('#skillUninstallBtn'), skHint = $('#skillHint');
  if(!invoke){ [cliBtn, skBtn, skUn].forEach(b => { if (b) b.disabled = true; }); return; }
  try{
    const [cli, sk] = await Promise.all([invoke('cli_install_status'), invoke('skills_status')]);
    if (cli.installed){ agentBadge(cliBadge, '已安装', 'ok'); cliBtn.textContent = '重新安装'; }
    else { agentBadge(cliBadge, cli.source_available ? '未安装' : '未打包', 'idle'); cliBtn.textContent = '安装'; }
    cliBtn.disabled = !cli.source_available;
    cliHint.textContent = cli.target_path
      ? ('安装位置: ' + cli.target_path + (cli.installed ? '' : ' · 安装后需重开终端使 PATH 生效'))
      : '';
    if (sk.state === 'up-to-date') agentBadge(skBadge, '已安装 · 最新', 'ok');
    else if (sk.state === 'drift') agentBadge(skBadge, '有更新', 'warn');
    else agentBadge(skBadge, '未安装', 'idle');
    skBtn.textContent = sk.state === 'up-to-date' ? '重新同步' : sk.state === 'drift' ? '更新' : '安装技能';
    skUn.disabled = sk.state === 'not-installed';
    skHint.textContent = '安装到 ' + sk.targets.map(t => '~/' + t.label + '/skills').join('、');
  }catch(e){ /* 状态读取失败静默，按钮保持当前态 */ }
}
$('#cliInstallBtn')?.addEventListener('click', async () => {
  const b = $('#cliInstallBtn');
  b.disabled = true;
  try{
    await invoke('install_cli');
    toast('CLI 已安装', '重开终端后即可使用 qingniao 命令');
  }catch(e){ toast('CLI 安装失败', String(e && e.message || e), 'err'); }
  refreshAgentStatus();
});
$('#skillInstallBtn')?.addEventListener('click', async () => {
  const b = $('#skillInstallBtn');
  b.disabled = true;
  try{
    await invoke('install_skills');
    toast('技能已同步', 'Claude / Codex 等 Agent 即可发现青鸟技能');
  }catch(e){ toast('技能同步失败', String(e && e.message || e), 'err'); }
  refreshAgentStatus();
});
$('#skillUninstallBtn')?.addEventListener('click', async () => {
  const b = $('#skillUninstallBtn');
  b.disabled = true;
  try{
    await invoke('uninstall_skills');
    toast('技能已卸载', '只移除了青鸟托管的文件');
  }catch(e){ toast('卸载失败', String(e && e.message || e), 'err'); }
  refreshAgentStatus();
});

$('#toggleSecret')?.addEventListener('click', () => {
  const inp = $('#secretInput');
  const showing = inp.type === 'text';
  inp.type = showing ? 'password' : 'text';
  $('#toggleSecret').textContent = showing ? '显示' : '隐藏';
});

document.addEventListener('keydown', e => {
  if (e.key !== 'Escape') return;
  if (expand.classList.contains('show')){ closeExpand(); return; }
  closeAddMenu();
  if (rvOverlay.classList.contains('show')){ closeRetrieve(); return; }
  $$('.overlay').forEach(o => o.classList.remove('show'));
});

/* ===================== 配置 · 飞书应用 pane ===================== */
const connCard = $('#connCard'), connIcon = $('#connIcon'), connHeadTxt = $('#connHeadTxt'),
      connTime = $('#connTime'), connGrid = $('#connGrid'), readyBadge = $('#readyBadge'),
      connPermHint = $('#connPermHint'),
      testConnBtn = $('#testConnBtn'), testConnLabel = $('#testConnLabel');
const WARN_SVG = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10.3 3.9L1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>';
// 探测型权限元信息：key → 徽标元素、开通/未开通文案、缺失时的警告块文案
const PERM_META = {
  upload: {
    badge: $('#permUploadBadge'), okText: '已开通', missText: '未开通',
    title: '缺少图片上传权限',
    desc: '上传图片需要 <code>im:resource:upload</code> 权限。点击下方按钮前往飞书开放平台申请，申请后必须发布新版本才会生效。',
    btn: '申请 im:resource:upload 权限',
    ok: null, applyUrl: '',
  },
  drive: {
    badge: $('#permDriveBadge'), okText: '已开通', missText: '未开通',
    title: '缺少云空间权限',
    desc: '两端机器无法直连时，青鸟需要用飞书云空间中转文件。缺 <code>drive:drive</code> 权限的这段时间，只有同一网络内的机器能互传。申请后必须发布新版本才会生效。',
    btn: '申请 drive:drive 权限',
    ok: null, applyUrl: '',
  },
};
const CONN_ICON_OK   = '<path d="M20 6L9 17l-5-5"/>';
const CONN_ICON_IDLE = '<path d="M5 12h14"/>';
const CONN_ICON_ERR  = '<circle cx="12" cy="12" r="10"/><path d="M12 8v4"/><path d="M12 16h.01"/>';
function setConnCard(state, text, time){
  connCard.classList.remove('idle','ok','err');
  connCard.classList.add(state);
  connIcon.innerHTML = state === 'ok' ? CONN_ICON_OK : state === 'err' ? CONN_ICON_ERR : CONN_ICON_IDLE;
  connHeadTxt.textContent = text;
  connTime.textContent = time || '';
  connGrid.hidden = state !== 'ok';
  if(state !== 'ok'){ resetPerms(); }
  refreshReadyBadge();
}
function setPermStatus(key, state){ // state: 'pending' | 'ok' | 'miss'
  const m = PERM_META[key]; if(!m) return;
  m.badge.className = 'sbadge ' + (state === 'ok' ? 'ok' : state === 'miss' ? 'warn' : 'neutral');
  m.badge.textContent = state === 'ok' ? m.okText : state === 'miss' ? m.missText : '检测中…';
  m.ok = state === 'pending' ? null : state === 'ok';
  refreshReadyBadge();
}
function refreshReadyBadge(){
  if (connGrid.hidden){ readyBadge.hidden = true; return; }
  // tenant_access_token / bot:v3:info 为隐式校验，连接成功即计入就绪数
  const okN = 2 + (PERM_META.upload.ok === true) + (PERM_META.drive.ok === true);
  readyBadge.hidden = false;
  readyBadge.textContent = okN + ' / 4 项就绪';
  readyBadge.className = 'sc-count' + (okN === 4 ? '' : ' warn');
}
function refreshWarns(){
  connPermHint.innerHTML = '';
  const missing = Object.keys(PERM_META).filter(k => PERM_META[k].ok === false);
  if (!missing.length){ connPermHint.hidden = true; return; }
  connPermHint.hidden = false;
  for (const key of missing){
    const m = PERM_META[key];
    const block = document.createElement('div');
    block.className = 'sc-issue';
    block.innerHTML = `<div class="issue-head">${WARN_SVG}<span>${m.title}</span></div><p>${m.desc}</p>`;
    const row = document.createElement('div');
    row.className = 'action-row';
    const btn = document.createElement('button');
    btn.className = 'btn-secondary';
    btn.innerHTML = `${m.btn} <span aria-hidden="true">↗</span>`;
    btn.onclick = () => openExternal(m.applyUrl);
    row.appendChild(btn);
    block.appendChild(row);
    connPermHint.appendChild(block);
  }
}
function resetPerms(){
  Object.keys(PERM_META).forEach(k => {
    PERM_META[k].applyUrl = '';
    setPermStatus(k, 'pending');
  });
  refreshWarns();
}
function updatePerm(key, has, applyUrl){
  const m = PERM_META[key]; if(!m) return;
  m.applyUrl = applyUrl || '';
  setPermStatus(key, has ? 'ok' : 'miss');
  refreshWarns();
}
let testingConn = false;
async function runConnTest(){
  if(testingConn) return;
  const appId = $('#appIdInput').value.trim();
  const appSecret = $('#secretInput').value.trim();
  if(!appId || !appSecret){
    setConnCard('err', '连接失败 · 请先填写 App ID 和 App Secret');
    return;
  }
  testingConn = true;
  testConnBtn.disabled = true;
  testConnLabel.textContent = '测试中…';
  try{
    const result = await invoke('test_connection', {appId, appSecret});
    setConnCard('ok', '已连接 · ' + result.app_name, '刚刚校验');
    (result.permissions || []).forEach(p => updatePerm(p.key, p.ok, p.apply_url));
  }catch(e){
    const msg = typeof e === 'string' ? e : (e && e.message) || '未知错误';
    setConnCard('err', '连接失败 · ' + msg);
    log('error', 'test_connection: 连接失败 ' + msg);
  }finally{
    testingConn = false;
    testConnBtn.disabled = false;
    testConnLabel.textContent = '测试连接';
  }
}
testConnBtn.addEventListener('click', runConnTest);

async function openExternal(url){
  if(invoke){
    try{ await invoke('plugin:opener|open_url', {url}); return; }
    catch(e){ log('error', '打开外部链接失败: ' + (e.message || e)); }
  }
  window.open(url, '_blank', 'noopener');
}
$('#howToCredsLink')?.addEventListener('click', e => {
  e.preventDefault();
  openExternal('https://open.feishu.cn/document/faq/trouble-shooting/how-to-obtain-app-id');
});
$('#openFeishuPlatform')?.addEventListener('click', () => openExternal('https://open.feishu.cn/app'));

function loadConfigForm(){
  $('#appIdInput').value = config.app_id || '';
  $('#secretInput').value = config.app_secret || '';
  setConnCard('idle', '未连接 · 填写凭证后点击「测试连接」');
  if(config.app_id && config.app_secret) setTimeout(runConnTest, 150);
  $('#portInput').value = config.transfer.configured_port;
  $('#downloadDirPath').textContent = config.transfer.download_dir || '系统下载目录';
  refreshKeyCard();
  refreshServiceStatus();
}
async function saveConfigForm(){
  config.app_id = $('#appIdInput').value.trim();
  config.app_secret = $('#secretInput').value.trim();
  saveConfig();
}

/* ===================== 配置 · 文件传输 pane ===================== */
function setPortNow(state, label, bound){
  const now = $('#portNow');
  now.classList.toggle('err', state === 'err');
  $('.plbl', now).textContent = label;
  $('#portBound').textContent = bound;
}
async function refreshServiceStatus(){
  if(!invoke){ setPortNow('err', '未连接', 'Tauri 不可用'); return; }
  try{
    const st = await invoke('transfer_service_status');
    if (st.status === 'Running') setPortNow('', '实际绑定', '127.0.0.1:' + st.bound_port);
    else if (st.status === 'Failed') setPortNow('err', '端口被占用', (st.requested_port || st.bound_port || '') + ' 端口不可用，换一个再重启');
    else if (st.status === 'Starting') setPortNow('', '启动中', '…');
    else setPortNow('err', '未运行', '本地服务未启动');
  }catch(e){
    setPortNow('err', '未运行', '传输服务尚未接入（P3）');
  }
}
$('#portRestart').addEventListener('click', async () => {
  const p = ($('#portInput').value || '').trim();
  if (!/^\d+$/.test(p) || +p < 1024 || +p > 65535){
    setPortNow('err', '端口无效', '请填 1024–65535 的高位端口');
    return;
  }
  if(!invoke) return;
  try{
    config.transfer.configured_port = +p;
    saveConfig();
    const st = await invoke('transfer_service_restart', {port: +p});
    if (st.status === 'Running'){
      setPortNow('', '实际绑定', '127.0.0.1:' + st.bound_port);
      toast('本地服务已重启', '端口 ' + st.bound_port);
    } else {
      setPortNow('err', '端口被占用', p + ' 被占用，换一个再重启');
    }
  }catch(e){
    setPortNow('err', '端口被占用', String(e).slice(0, 60));
  }
});
$('#dirPick').addEventListener('click', async () => {
  if(!invoke) return;
  try{
    const dir = await invoke('plugin:dialog|open', {options: {directory: true, multiple: false, title: '选择下载目录'}});
    if (dir){
      config.transfer.download_dir = dir;
      $('#downloadDirPath').textContent = dir;
      saveConfig();
    }
  }catch(e){ toast('目录选择不可用', String(e).slice(0,60), 'err'); }
});

/* 密钥卡 */
async function refreshKeyCard(){
  if(!invoke) return;
  try{
    const st = await invoke('key_status');
    const has = st.has_key;
    $('#keyHeadTitle').textContent = has ? '传输密钥 · 已配对' : '传输密钥 · 未配置';
    $('#keyFp').hidden = !has;
    if (has){
      $('#keyFp').textContent = '指纹 ' + st.fingerprint;
      $('#keyStoreHint').textContent = (st.has_previous ? '存在过渡旧密钥 · ' : '') + '存于系统凭据库';
    } else {
      $('#keyStoreHint').textContent = '尚未生成或导入主密钥';
    }
    $('[data-key="export"]').hidden = !has;
    $('[data-key="rotate"]').hidden = !has;
    $('#keyGenerateBtn').textContent = has ? '重新生成' : '生成密钥';
  }catch(e){
    $('#keyStoreHint').textContent = '密钥服务尚未接入（P1）';
  }
}
const keyCard = $('#keyCard');
keyCard.addEventListener('click', e => {
  const b = e.target.closest('[data-key]');
  if (!b || b.hidden) return;
  const k = b.dataset.key;
  if (k === 'export'){ keyCard.setAttribute('data-slot', keyCard.dataset.slot === 'export' ? '' : 'export'); doKeyExport(); return; }
  keyCard.setAttribute('data-slot', keyCard.dataset.slot === k ? '' : k);
});
async function doKeyExport(){
  try{
    const hex = await invoke('key_export');
    const code = $('#keyPlain');
    code.dataset.plain = hex;
    code.textContent = '•••• •••• •••• •••• •••• •••• •••• ••••';
    code.classList.add('masked');
    $('#keyReveal').textContent = '显示';
  }catch(e){ toast('导出失败', String(e).slice(0,80), 'err'); }
}
$('#keyReveal').addEventListener('click', () => {
  const code = $('#keyPlain');
  if (!code.dataset.plain) return;
  const masked = code.classList.contains('masked');
  code.classList.toggle('masked', !masked);
  code.textContent = masked ? code.dataset.plain : '•••• •••• •••• •••• •••• •••• •••• ••••';
  $('#keyReveal').textContent = masked ? '隐藏' : '显示';
});
$('#keyCopy').addEventListener('click', e => {
  const code = $('#keyPlain');
  if (code.dataset.plain && navigator.clipboard) navigator.clipboard.writeText(code.dataset.plain).catch(() => {});
  e.currentTarget.textContent = '已复制';
  setTimeout(() => { e.currentTarget.textContent = '复制'; }, 1600);
});
$('#keyImportInput').addEventListener('input', function(){
  const v = this.value.trim();
  const box = $('#keyImportFp');
  box.hidden = !v;
  const valid = /^[0-9a-fA-F]{64}$/.test(v);
  if (valid){
    importFpOk = true;
    box.classList.remove('bad');
    $('#keyImportFpText').innerHTML = '指纹 <code>' + v.slice(0,8).toLowerCase() + '</code> —— 请与另一台机器显示的指纹逐位核对后再导入';
    $('#keyImportConfirm').disabled = false;
  } else {
    importFpOk = false;
    box.classList.add('bad');
    $('#keyImportFpText').textContent = '还不是一把完整的密钥：需要 64 位十六进制字符，当前 ' + v.length + ' 位';
    $('#keyImportConfirm').disabled = true;
  }
});
$('#keyGenerateConfirm').addEventListener('click', async () => {
  if(!invoke) return;
  try{
    const st = await invoke('key_generate');
    toast('密钥已生成', '指纹 ' + st.fingerprint);
    keyCard.setAttribute('data-slot','');
    refreshKeyCard();
  }catch(e){ toast('生成失败', String(e).slice(0,80), 'err'); }
});
$('#keyImportConfirm').addEventListener('click', async () => {
  if(!invoke || !importFpOk) return;
  try{
    const st = await invoke('key_import', {hex: $('#keyImportInput').value.trim()});
    toast('密钥已导入', '指纹 ' + st.fingerprint);
    $('#keyImportInput').value = '';
    $('#keyImportFp').hidden = true;
    keyCard.setAttribute('data-slot','');
    refreshKeyCard();
  }catch(e){ toast('导入失败', String(e).slice(0,80), 'err'); }
});
$('#keyRotateConfirm').addEventListener('click', async () => {
  if(!invoke) return;
  try{
    const st = await invoke('key_rotate');
    toast('密钥已轮换', '新指纹 ' + st.fingerprint + ' · 旧密钥保留 30 天');
    keyCard.setAttribute('data-slot','');
    refreshKeyCard();
  }catch(e){ toast('轮换失败', String(e).slice(0,80), 'err'); }
});

/* 服务状态事件（后端推送） */
listenEvent('transfer://state', ev => {
  const p = ev.payload || {};
  if (p.status === 'Running') setPortNow('', '实际绑定', '127.0.0.1:' + p.bound_port);
  else if (p.status === 'Failed') setPortNow('err', '端口被占用', '换一个端口再重启');
});

/* ===================== 偏好 ===================== */
function initPrefControls(){
  // 智能识别默认
  $$('#segDetect button').forEach(b => {
    b.classList.toggle('active', b.dataset.v === (config.last_type || 'auto'));
    b.onclick = () => {
      $$('#segDetect button').forEach(x => x.classList.remove('active'));
      b.classList.add('active');
      config.last_type = b.dataset.v;
      saveConfig(); refresh();
    };
  });
  // 发送快捷键
  $$('#segHotkey button').forEach(b => {
    b.classList.toggle('active', b.dataset.v === hotkeyMode);
    b.onclick = () => {
      $$('#segHotkey button').forEach(x => x.classList.remove('active'));
      b.classList.add('active');
      hotkeyMode = b.dataset.v;
      config.hotkey = hotkeyMode;
      saveConfig();
    };
  });
  // 开关（提示音 / 粘贴自动插入）暂存于 config.prefs
  const prefs = config.prefs || (config.prefs = {sound: true, paste: true});
  [['#swSound','sound'],['#swPaste','paste']].forEach(([sel,key]) => {
    const el = $(sel);
    el.classList.toggle('on', prefs[key] !== false);
    el.onclick = () => {
      el.classList.toggle('on');
      prefs[key] = el.classList.contains('on');
      saveConfig();
    };
  });
  $('#residentSwitch').onclick = function(){ this.classList.toggle('on'); };
}
$$('.switch').forEach(s => { if (!s.onclick) s.addEventListener('click', () => s.classList.toggle('on')); });

/* ===================== 初始化 ===================== */
async function init(){
  await loadConfig();
  applyTheme(config.theme || 'auto');
  initThemeCards();
  initPrefControls();
  renderBotMenu();
  updateBotPicker();
  renderStream();
  refresh();
  // 动态填充关于页版本号
  try {
    const ver = await TAURI.app.getVersion();
    const el = document.getElementById('aboutVersion');
    if(el && ver) el.textContent = 'v' + ver;
  } catch(e) {}
  log('info', '前端初始化完成 · history=' + config.history.length);
}
init();

})();
