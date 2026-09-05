/* 青鸟 · 飞书消息助手 — Tauri 前端 */
(function(){
'use strict';

/* ===================== 工具 ===================== */
const $  = (s, r=document) => r.querySelector(s);
const $$ = (s, r=document) => Array.from(r.querySelectorAll(s));
const esc = s => String(s).replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const uid = () => 'a' + (++attIdSeq) + '_' + Date.now().toString(36);

/* ===================== Tauri 封装 ===================== */
const invoke = window.__TAURI__ ? window.__TAURI__.core.invoke : null;
if(!invoke){ console.warn('Tauri 不可用，部分功能将降级'); }

/* ===================== 状态 ===================== */
let config = null;
let attachments = [];
let forcedType = 'auto';
let attIdSeq = 0;

/* ===================== DOM 引用 ===================== */
const editor    = $('#editor');
const attStrip  = $('#attachments');
const counter   = $('#counter');
const typePill  = $('#typePill');
const whyText   = $('#whyText');
const sendBtn   = $('.send');
const resultEl  = $('.result');
const historyUl = $('.history ul');
const segBtns   = $$('.intent .seg button');

/* ===================== 配置加载/保存 ===================== */
async function loadConfig(){
  if(invoke){
    try{
      config = await invoke('load_config');
    }catch(e){
      console.error('加载配置失败:', e);
      config = defaultConfig();
    }
  } else {
    config = defaultConfig();
  }
  if(!config.webhooks) config.webhooks = [];
  if(!config.history) config.history = [];
  if(config.last_webhook === undefined || config.last_webhook === null) config.last_webhook = 0;
  if(!config.last_type) config.last_type = 'auto';
  if(!config.app_id) config.app_id = '';
  if(!config.app_secret) config.app_secret = '';
  forcedType = config.last_type || 'auto';
}
function defaultConfig(){
  return {webhooks:[], history:[], last_webhook:0, last_type:'auto', app_id:'', app_secret:''};
}
async function saveConfig(){
  if(invoke){
    try{ await invoke('save_config', {config}); }
    catch(e){ console.error('保存配置失败:', e); }
  }
}

/* ===================== 签名（Web Crypto） ===================== */
async function hmacSign(secret, timestamp){
  const key = timestamp + '\n' + secret;
  const enc = new TextEncoder();
  const ck = await crypto.subtle.importKey('raw', enc.encode(key), {name:'HMAC', hash:'SHA-256'}, false, ['sign']);
  const sig = await crypto.subtle.sign('HMAC', ck, enc.encode(''));
  return btoa(String.fromCharCode(...new Uint8Array(sig)));
}

/* ===================== Markdown → 飞书 post ===================== */
function parseInline(text){
  const out = [];
  let i = 0;
  while(i < text.length){
    const rest = text.slice(i);
    const atM = rest.match(/^<at\s+user_id="([^"]+)"[^>]*>([^<]*)<\/at>/);
    if(atM){ out.push({tag:'at', user_id:atM[1], user_name:atM[2]||''}); i += atM[0].length; continue; }
    if(rest.startsWith('@所有人')){ out.push({tag:'at', user_id:'all', user_name:'所有人'}); i += 4; continue; }
    const imgM = rest.match(/^!\[([^\]]*)\]\(([^)]+)\)/);
    if(imgM){ out.push({tag:'img', image_key:imgM[2]}); i += imgM[0].length; continue; }
    const linkM = rest.match(/^\[([^\]]+)\]\(([^)]+)\)/);
    if(linkM){ out.push({tag:'a', text:linkM[1], href:linkM[2]}); i += linkM[0].length; continue; }
    const boldM = rest.match(/^\*\*([^*]+)\*\*/);
    if(boldM){ out.push({tag:'text', text:boldM[1]}); i += boldM[0].length; continue; }
    const codeM = rest.match(/^`([^`]+)`/);
    if(codeM){ out.push({tag:'text', text:codeM[1]}); i += codeM[0].length; continue; }
    const next = rest.search(/(\*\*|\[|<at|@所有人|!\[|`)/);
    if(next === -1){ out.push({tag:'text', text:rest}); break; }
    if(next > 0){ out.push({tag:'text', text:rest.slice(0,next)}); i += next; }
    else { out.push({tag:'text', text:text[i]}); i++; }
  }
  return out;
}

function mdToPost(md, attImageKeys){
  const lines = md.split('\n');
  let title = '';
  const content = [];
  for(const line of lines){
    if(!title && line.startsWith('# ')){ title = line.slice(2).trim(); continue; }
    if(line.trim() === '') continue;
    const inline = parseInline(line);
    if(inline.length) content.push(inline);
  }
  for(const key of attImageKeys) content.push([{tag:'img', image_key:key}]);
  return {msg_type:'post', content:{post:{zh_cn:{title, content}}}};
}

/* ===================== 智能识别 ===================== */
function detect(text, attCount){
  const hasImg = attCount > 0;
  const hasMd  = /(\*\*[^*]+\*\*|`[^`]+`|\[[^\]]+\]\([^)]+\)|^#{1,3}\s|^[-*]\s|^>\s|^\d+\.\s)/m.test(text);
  const hasAt  = /<at\s+user_id=|@所有人/.test(text);
  const hasCard = /```card|\{\s*"config"|\{\s*"header"/.test(text);
  if(hasCard) return {t:'interactive', label:'交互卡片', why:'检测到 <b>卡片 JSON 或按钮定义</b>，将以交互卡片渲染'};
  if(hasImg && !text.trim()) return {t:'image', label:'图片', why:'仅包含 <b>'+attCount+' 张图片</b>，采用图片消息发送'};
  if(hasImg || hasMd || hasAt){
    const r = [hasMd&&'Markdown', hasAt&&'@ 提及', hasImg&&'图片附件'].filter(Boolean).join('、');
    return {t:'post', label:'富文本', why:'检测到 <b>'+r+'</b>，纯文本无法完整呈现'};
  }
  if(!text.trim()) return {t:'text', label:'文本', why:'编辑器为空 · 输入内容后会自动判断'};
  return {t:'text', label:'文本', why:'纯文字消息，直接以 text 类型发送'};
}

function refreshIntent(){
  const t = editor.value;
  counter.textContent = t.replace(/\s/g,'').length + ' 字';
  const d = detect(t, attachments.length);
  const finalType = forcedType === 'auto' ? d.t : forcedType;
  typePill.className = 'type-pill ' + (finalType === 'interactive' ? 'card' : finalType);
  const labelMap = {text:'文本', post:'富文本', image:'图片', interactive:'交互卡片'};
  typePill.textContent = labelMap[finalType] || '文本';
  if(forcedType === 'auto'){ whyText.innerHTML = d.why; }
  else {
    const same = d.t === forcedType;
    whyText.innerHTML = same ? '与智能识别一致 · '+d.why
      : '已手动指定为 <b>'+labelMap[forcedType]+'</b>（识别结果：'+labelMap[d.t]+'）';
  }
}

/* ===================== 消息组装 ===================== */
function extractCardJson(text){
  let t = text.trim().replace(/^```card\s*/i,'').replace(/```\s*$/,'').trim();
  return JSON.parse(t);
}
function buildPayload(text, type){
  const imgKeys = attachments.filter(a => a.imageKey).map(a => a.imageKey);
  switch(type){
    case 'text':  return {msg_type:'text', content:{text}};
    case 'post':  return mdToPost(text, imgKeys);
    case 'image':
      if(!imgKeys.length) throw new Error('没有可用的 image_key，请等待图片上传完成');
      return {msg_type:'image', content:{image_key:imgKeys[0]}};
    case 'interactive':
      return {msg_type:'interactive', card: extractCardJson(text)};
  }
}

/* ===================== 发送 ===================== */
async function send(){
  const bot = config.webhooks[config.last_webhook];
  if(!bot){ setResult(false,'未配置机器人'); return; }
  const text = editor.value;
  const d = detect(text, attachments.length);
  const type = forcedType === 'auto' ? d.t : forcedType;
  if(type !== 'image' && !text.trim() && attachments.length === 0){ setResult(false,'内容为空'); return; }

  const uploading = attachments.filter(a => a.status === 'uploading');
  if(uploading.length){ setResult(false,'图片仍在上传中，请稍候'); return; }

  let payload;
  try{ payload = buildPayload(text, type); }
  catch(e){ setResult(false,'消息组装失败：'+e.message); return; }

  if(bot.secret){
    const ts = Math.floor(Date.now()/1000).toString();
    payload.timestamp = ts;
    payload.sign = await hmacSign(bot.secret, ts);
  }

  sendBtn.disabled = true; sendBtn.style.opacity = '0.6';
  setResult(null,'发送中…');
  const t0 = performance.now();

  try{
    let respText;
    if(invoke){
      respText = await invoke('send_webhook', {url: bot.url, payload});
    } else {
      // 降级：浏览器 fetch
      const resp = await fetch(bot.url, {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify(payload)});
      respText = 'HTTP ' + resp.status + ': ' + await resp.text();
    }
    const dt = Math.round(performance.now() - t0);
    // 解析 "HTTP 200: {json}"
    const m = respText.match(/^HTTP (\d+):\s*(.*)$/s);
    const httpStatus = m ? parseInt(m[1]) : 0;
    const body = m ? m[2] : respText;
    let ok = httpStatus >= 200 && httpStatus < 300;
    let msg = 'HTTP ' + httpStatus;
    try{
      const j = JSON.parse(body);
      if(j.code === 0){ ok = true; msg = '200 OK'; }
      else if(j.code !== undefined){ ok = false; msg = j.code + ' ' + (j.msg||''); }
    }catch(e){}
    setResult(ok, msg + ' · ' + dt + 'ms');
    addHistory({time: fmtTime(new Date()), msg_type: type, summary: makePreview(text, type), ok, status: msg, payload});
  }catch(e){
    setResult(false,'发送失败：'+e.message);
    addHistory({time: fmtTime(new Date()), msg_type: type, summary: makePreview(text, type), ok:false, status:'发送失败', payload:{}});
  }finally{
    sendBtn.disabled = false; sendBtn.style.opacity = '';
  }
}
function makePreview(text, type){
  if(type === 'image') return attachments.map(a=>a.name).join(', ')+' ('+attachments.length+' 张附件)';
  if(type === 'interactive') return '交互卡片消息';
  return (text.split('\n')[0]||'').slice(0,60) || '(空)';
}

/* ===================== 结果 toast ===================== */
function setResult(ok, msg){
  resultEl.classList.remove('ok','err');
  if(ok === true) resultEl.classList.add('ok');
  else if(ok === false) resultEl.classList.add('err');
  const span = resultEl.querySelector('span:nth-child(2)');
  const code = resultEl.querySelector('code');
  if(ok === null){ span.innerHTML = '<b>'+esc(msg)+'</b>'; if(code) code.textContent = ''; }
  else {
    span.innerHTML = ok ? '上次发送 <b style="color:var(--fg);font-weight:500">成功</b>' : '上次发送 <b style="color:var(--danger);font-weight:500">失败</b>';
    if(code) code.textContent = msg;
  }
}

/* ===================== 发送历史 ===================== */
function fmtTime(d){
  const pad = n => String(n).padStart(2,'0');
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  if(sameDay) return '今天 '+pad(d.getHours())+':'+pad(d.getMinutes());
  const yest = new Date(now); yest.setDate(now.getDate()-1);
  if(d.toDateString() === yest.toDateString()) return '昨天 '+pad(d.getHours())+':'+pad(d.getMinutes());
  return (d.getMonth()+1)+'/'+d.getDate()+' '+pad(d.getHours())+':'+pad(d.getMinutes());
}
function renderHistory(){
  historyUl.innerHTML = '';
  if(!config.history.length){
    historyUl.innerHTML = '<li style="color:var(--muted-2);font-size:12px;padding:16px;text-align:center">暂无发送记录</li>';
    return;
  }
  const typeMap = {text:'文本', post:'富文本', image:'图片', interactive:'交互卡片'};
  for(const h of config.history.slice().reverse()){
    const li = document.createElement('li');
    li.className = h.ok ? 'ok' : 'err';
    li.innerHTML = '<span class="rail"></span><div>'
      + '<div class="head"><span class="time">'+esc(h.time)+'</span>'
      + '<span class="kind '+(h.msg_type==='interactive'?'card':h.msg_type)+'">'+esc(typeMap[h.msg_type]||h.msg_type)+'</span></div>'
      + '<div class="preview">'+esc(h.summary||'')+'</div>'
      + '<div class="status">'+(h.ok?'✓ ':'✗ ')+esc(h.status||'')+'</div></div>';
    historyUl.appendChild(li);
  }
}
async function addHistory(h){
  config.history.push(h);
  if(config.history.length > 50) config.history.shift();
  await saveConfig();
  renderHistory();
}
$('.history header button')?.addEventListener('click', async () => {
  if(!config.history.length) return;
  config.history = [];
  await saveConfig();
  renderHistory();
  setResult(null, '已清空发送历史');
});

/* ===================== 附件 ===================== */
function renderAttachments(){
  $$('.att', attStrip).forEach(el => el.remove());
  const addBtn = $('.att-add', attStrip);
  for(const att of attachments){
    const div = document.createElement('div');
    div.className = 'att' + (att.status === 'uploading' ? ' uploading' : '') + (att.status === 'error' ? ' missing' : '');
    div.style.backgroundImage = 'url('+att.dataUrl+')';
    div.dataset.id = att.id;
    let stateHtml = '';
    if(att.status === 'uploading') stateHtml = '<span class="state"><span class="sd"></span>上传中…</span>';
    else if(att.status === 'done') stateHtml = '<span class="state"><span class="sd"></span>'+esc((att.imageKey||'').slice(0,12)+'…')+'</span>';
    else if(att.status === 'error') stateHtml = '<span class="state"><span class="sd"></span>点击手动填 key</span>';
    else stateHtml = '<span class="state"><span class="sd"></span>待上传</span>';
    div.innerHTML = '<span class="badge">'+esc(att.name)+'</span>'+stateHtml+'<button class="rm" aria-label="移除附件">×</button>';
    div.addEventListener('click', (e) => {
      if(e.target.closest('.rm')) return;
      if(att.status === 'error' || att.status === 'manual' || !att.imageKey){
        const key = prompt('请输入 image_key：', att.imageKey || '');
        if(key){ att.imageKey = key.trim(); att.status = 'done'; renderAttachments(); refreshIntent(); }
      }
    });
    attStrip.insertBefore(div, addBtn);
  }
  attStrip.style.display = attachments.length ? '' : 'none';
}
function addAttachment(file, dataUrl){
  const att = {id: uid(), name: file.name || 'pasted.png', dataUrl, imageKey:'', status:'uploading', error:''};
  attachments.push(att);
  renderAttachments(); refreshIntent();
  uploadAttachment(att);
}
function removeAttachment(id){
  attachments = attachments.filter(a => a.id !== id);
  renderAttachments(); refreshIntent();
}
async function uploadAttachment(att){
  if(invoke && config.app_id && config.app_secret){
    try{
      const base64 = att.dataUrl.split(',')[1];
      const key = await invoke('upload_image', {
        imageBase64: base64, filename: att.name,
        appId: config.app_id, appSecret: config.app_secret
      });
      att.imageKey = key; att.status = 'done';
    }catch(e){
      att.status = 'error'; att.error = e.message;
    }
  } else {
    // 无 Tauri 或未配置应用凭证：降级为手动填 key
    await new Promise(r => setTimeout(r, 800));
    att.status = 'error';
    att.error = '未配置飞书应用凭证';
  }
  renderAttachments(); refreshIntent();
}
attStrip.addEventListener('click', (e) => {
  const rm = e.target.closest('.rm');
  if(rm){ const id = rm.closest('.att').dataset.id; removeAttachment(id); }
});
editor.addEventListener('paste', (e) => {
  const items = e.clipboardData && e.clipboardData.items;
  if(!items) return;
  for(const it of items){
    if(it.type && it.type.startsWith('image/')){
      e.preventDefault();
      const f = it.getAsFile();
      const r = new FileReader();
      r.onload = ev => addAttachment(f, ev.target.result);
      r.readAsDataURL(f);
    }
  }
});
$('.att-add', attStrip)?.addEventListener('click', () => {
  const inp = document.createElement('input');
  inp.type = 'file'; inp.accept = 'image/*'; inp.multiple = true;
  inp.onchange = () => {
    for(const f of inp.files){
      const r = new FileReader();
      r.onload = ev => addAttachment(f, ev.target.result);
      r.readAsDataURL(f);
    }
  };
  inp.click();
});

/* ===================== 机器人选择 & 管理 ===================== */
function renderBotMenu(){
  const menu = $('#botMenu');
  $$('.bot-item', menu).forEach(el => el.remove());
  const actionBtn = $('#menuGoManage');
  config.webhooks.forEach((bot, idx) => {
    const btn = document.createElement('button');
    btn.className = 'bot-item' + (idx === config.last_webhook ? ' active' : '');
    btn.dataset.name = bot.name;
    btn.dataset.hint = bot.secret ? '已设置签名' : '未设置签名';
    btn.setAttribute('role','menuitemradio');
    btn.setAttribute('aria-checked', String(idx === config.last_webhook));
    const shortUrl = (bot.url||'').replace('https://open.feishu.cn/open-apis/bot/v2/hook/','open.feishu.cn/…/');
    btn.innerHTML = '<span class="bdot'+(bot.secret?'':' muted')+'" aria-hidden="true"></span>'
      + '<div class="binfo"><b>'+esc(bot.name)+'</b><span>'+esc(shortUrl)+'</span></div>'
      + '<svg class="check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6L9 17l-5-5"/></svg>';
    btn.addEventListener('click', async () => {
      config.last_webhook = idx; await saveConfig();
      renderBotMenu(); updateBotPicker(); toggleBotMenu(false);
    });
    menu.insertBefore(btn, actionBtn);
  });
  const title = $('.bot-menu-title', menu);
  if(title) title.textContent = '发送目标 · '+config.webhooks.length+' 个机器人';
}
function updateBotPicker(){
  const bot = config.webhooks[config.last_webhook];
  $('#currentBotName').textContent = bot ? bot.name : '未设置';
  $('#currentBotHint').textContent = bot ? (bot.secret ? '已设置签名' : '未设置签名') : '';
}
function renderBotList(){
  const list = $('#botList');
  list.innerHTML = '';
  config.webhooks.forEach((bot, idx) => {
    const item = document.createElement('div');
    item.className = 'bot-list-item';
    item.dataset.bot = bot.name;
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
        + '<span class="edit-hint">修改会立即生效</span>'
        + '<button data-action="cancel-edit">取消</button>'
        + '<button class="primary" data-action="save-edit">保存修改</button></div></div>'
      + '<div class="bot-list-confirm">'
        + '<span class="msg">确认删除 <b>'+esc(bot.name)+'</b>？</span>'
        + '<button data-action="cancel-delete">取消</button>'
        + '<button class="danger" data-action="confirm-delete">确认删除</button></div>';
    list.appendChild(item);
  });
  refreshBotCount();
}
function refreshBotCount(){
  const footerLeft = $('#manageOverlay footer .left');
  if(footerLeft) footerLeft.textContent = '共 '+config.webhooks.length+' 个机器人';
  const title = $('#botMenu .bot-menu-title');
  if(title) title.textContent = '发送目标 · '+config.webhooks.length+' 个机器人';
}
$('#botList').addEventListener('click', async (e) => {
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
    await saveConfig(); renderBotList(); renderBotMenu(); updateBotPicker();
  } else if(action === 'cancel-edit'){ item.classList.remove('editing'); }
  else if(action === 'delete'){
    $$('.bot-list-item').forEach(x => { if(x !== item) x.classList.remove('editing','confirming-delete'); });
    item.classList.remove('editing'); item.classList.add('confirming-delete');
  } else if(action === 'cancel-delete'){ item.classList.remove('confirming-delete'); }
  else if(action === 'confirm-delete'){
    config.webhooks.splice(idx, 1);
    if(config.last_webhook >= config.webhooks.length) config.last_webhook = Math.max(0, config.webhooks.length-1);
    await saveConfig(); renderBotList(); renderBotMenu(); updateBotPicker();
  }
});
document.querySelector('.add-bot-form .btn-primary')?.addEventListener('click', async () => {
  const inputs = $$('.add-bot-form input');
  const name = inputs[0].value.trim(), secret = inputs[1].value.trim(), url = inputs[2].value.trim();
  if(!name || !url){ alert('请填写名称和 Webhook 地址'); return; }
  config.webhooks.push({name, secret, url});
  await saveConfig(); renderBotList(); renderBotMenu(); updateBotPicker();
  inputs.forEach(i => i.value = '');
});

/* ===================== 弹窗控制 ===================== */
const configOverlay = $('#configOverlay');
const manageOverlay = $('#manageOverlay');
function openModal(el){ el.classList.add('show'); }
function closeModal(el){ el.classList.remove('show'); }
$('#openConfig').addEventListener('click', () => { loadConfigForm(); openModal(configOverlay); });
$('#closeConfig').addEventListener('click', () => closeModal(configOverlay));
$('#cancelConfig').addEventListener('click', () => closeModal(configOverlay));
$('#saveConfig').addEventListener('click', async () => { await saveConfigForm(); closeModal(configOverlay); });
configOverlay.addEventListener('click', e => { if(e.target === configOverlay) closeModal(configOverlay); });

$('#openManage').addEventListener('click', () => { renderBotList(); openModal(manageOverlay); });
$('#menuGoManage').addEventListener('click', () => { toggleBotMenu(false); renderBotList(); openModal(manageOverlay); });
$('#closeManage').addEventListener('click', () => closeModal(manageOverlay));
$('#closeManageFooter').addEventListener('click', () => closeModal(manageOverlay));
manageOverlay.addEventListener('click', e => { if(e.target === manageOverlay) closeModal(manageOverlay); });

document.addEventListener('keydown', e => { if(e.key === 'Escape'){ closeModal(configOverlay); closeModal(manageOverlay); } });

const botPickerBtn = $('#botPickerBtn');
const botMenu = $('#botMenu');
function toggleBotMenu(force){
  const show = typeof force === 'boolean' ? force : !botMenu.classList.contains('show');
  botMenu.classList.toggle('show', show);
  botPickerBtn.setAttribute('aria-expanded', String(show));
}
botPickerBtn.addEventListener('click', e => { e.stopPropagation(); toggleBotMenu(); });
document.addEventListener('click', e => {
  if(!botMenu.contains(e.target) && e.target !== botPickerBtn) toggleBotMenu(false);
});

const tabToPane = {app:'paneApp', pref:'panePref', about:'paneAbout'};
$$('#configOverlay .nav button').forEach(b => {
  b.addEventListener('click', () => {
    $$('#configOverlay .nav button').forEach(x => x.classList.remove('active'));
    b.classList.add('active');
    const target = tabToPane[b.dataset.tab];
    $$('#configOverlay .pane').forEach(p => p.classList.toggle('active', p.id === target));
  });
});

$('#toggleSecret')?.addEventListener('click', () => {
  const inp = $('#secretInput');
  const showing = inp.type === 'text';
  inp.type = showing ? 'password' : 'text';
  $('#toggleSecret').textContent = showing ? '显示' : '隐藏';
});

function loadConfigForm(){
  const appIdInput = $('#paneApp input[type="text"]');
  if(appIdInput) appIdInput.value = config.app_id || '';
  $('#secretInput').value = config.app_secret || '';
}
async function saveConfigForm(){
  const appIdInput = $('#paneApp input[type="text"]');
  config.app_id = appIdInput ? appIdInput.value.trim() : '';
  config.app_secret = $('#secretInput').value.trim();
  await saveConfig();
}

/* ===================== 主题 ===================== */
const themeMql = window.matchMedia('(prefers-color-scheme: dark)');
function applyTheme(pref){
  const resolved = pref === 'dark' || pref === 'light' ? pref : (themeMql.matches ? 'dark' : 'light');
  document.documentElement.dataset.theme = resolved;
  document.documentElement.dataset.themePref = pref;
}
applyTheme('auto');
$$('.theme-card').forEach(c => {
  c.addEventListener('click', () => {
    $$('.theme-card').forEach(x => { x.classList.remove('active'); x.setAttribute('aria-pressed','false'); });
    c.classList.add('active'); c.setAttribute('aria-pressed','true');
    applyTheme(c.dataset.theme);
  });
});
themeMql.addEventListener('change', () => {
  if((document.documentElement.dataset.themePref || 'auto') === 'auto') applyTheme('auto');
});

$$('.switch').forEach(s => s.addEventListener('click', () => s.classList.toggle('on')));
$$('.pref-seg').forEach(seg => {
  seg.querySelectorAll('button').forEach(b => b.addEventListener('click', () => {
    seg.querySelectorAll('button').forEach(x => x.classList.remove('active'));
    b.classList.add('active');
  }));
});

/* ===================== 工具栏 ===================== */
function wrapSelection(before, after){
  const start = editor.selectionStart, end = editor.selectionEnd;
  const val = editor.value;
  const selected = val.slice(start, end) || '文字';
  editor.value = val.slice(0,start) + before + selected + after + val.slice(end);
  editor.selectionStart = start + before.length;
  editor.selectionEnd = start + before.length + selected.length;
  editor.focus(); refreshIntent();
}
function insertAtCursor(text){
  const start = editor.selectionStart;
  editor.value = editor.value.slice(0,start) + text + editor.value.slice(editor.selectionEnd);
  editor.selectionStart = editor.selectionEnd = start + text.length;
  editor.focus(); refreshIntent();
}
$$('.toolbar .tool').forEach(btn => {
  btn.addEventListener('click', () => {
    const title = btn.getAttribute('title') || '';
    if(title.includes('加粗')) wrapSelection('**','**');
    else if(title.includes('斜体')) wrapSelection('*','*');
    else if(title.includes('标题')) insertAtCursor('\n# ');
    else if(title.includes('列表')) insertAtCursor('\n- ');
    else if(title.includes('链接')) wrapSelection('[','](https://)');
    else if(title.includes('代码')) wrapSelection('`','`');
    else if(title.includes('图片')) $('.att-add', attStrip)?.click();
    else if(title.includes('提及')) insertAtCursor('<at user_id="ou_all">所有人</at> ');
    else if(title.includes('斜杠') || title.includes('元素')) toggleSlashMenu();
  });
});

/* ===================== Slash 菜单 ===================== */
const slashMenu = $('#slashMenu');
function toggleSlashMenu(){ slashMenu.classList.toggle('show'); }
$$('#slashMenu .row').forEach(row => {
  row.addEventListener('click', () => {
    const label = row.querySelector('b').textContent;
    let template = '';
    if(label === '标题栏') template = '\n```card\n{"config":{"wide_screen_mode":true},"header":{"title":{"tag":"plain_text","content":"标题"},"template":"blue"},"elements":[{"tag":"div","text":{"tag":"lark_md","content":"内容"}}]}\n```';
    else if(label === '按钮组') template = '\n```card\n{"config":{"wide_screen_mode":true},"header":{"title":{"tag":"plain_text","content":"操作确认"},"template":"blue"},"elements":[{"tag":"action","actions":[{"tag":"button","text":{"tag":"plain_text","content":"确认"},"type":"primary"},{"tag":"button","text":{"tag":"plain_text","content":"取消"},"type":"default"}]}]}\n```';
    else if(label === '分栏布局') template = '\n```card\n{"config":{"wide_screen_mode":true},"elements":[{"tag":"column_set","columns":[{"tag":"column","elements":[{"tag":"div","text":{"tag":"plain_text","content":"左列"}}]},{"tag":"column","elements":[{"tag":"div","text":{"tag":"plain_text","content":"右列"}}]}]}]}\n```';
    insertAtCursor(template);
    slashMenu.classList.remove('show');
  });
});
editor.addEventListener('keydown', e => {
  if(e.key === '/' && !e.metaKey && !e.ctrlKey){
    setTimeout(() => { if(editor.value[editor.selectionStart-1] === '/') slashMenu.classList.add('show'); }, 0);
  }
  if(e.key === 'Escape') slashMenu.classList.remove('show');
});

/* ===================== 编辑/预览切换 ===================== */
$$('.mode button').forEach(btn => {
  btn.addEventListener('click', () => {
    $$('.mode button').forEach(x => { x.classList.remove('active'); x.setAttribute('aria-selected','false'); });
    btn.classList.add('active'); btn.setAttribute('aria-selected','true');
    if(btn.textContent.trim() === '预览') showPreview(); else hidePreview();
  });
});
function showPreview(){
  const wrap = $('.editor-wrap');
  let pv = $('#previewPane');
  if(!pv){
    pv = document.createElement('div');
    pv.id = 'previewPane';
    pv.style.cssText = 'flex:1;padding:18px 20px;overflow-y:auto;font-size:14.5px;line-height:1.65;white-space:pre-wrap;word-break:break-word;';
    wrap.appendChild(pv);
  }
  pv.innerHTML = mdToHtml(editor.value);
  pv.style.display = '';
  editor.style.display = 'none';
}
function hidePreview(){
  const pv = $('#previewPane');
  if(pv) pv.style.display = 'none';
  editor.style.display = '';
}
function mdToHtml(md){
  let html = esc(md);
  html = html.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  html = html.replace(/`([^`]+)`/g, '<code style="background:var(--surface-soft);padding:1px 5px;border-radius:4px;">$1</code>');
  html = html.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" style="color:var(--accent-ink);text-decoration:underline;" target="_blank">$1</a>');
  html = html.replace(/^# (.+)$/gm, '<h3 style="margin:8px 0;font-size:16px;">$1</h3>');
  html = html.replace(/\n/g, '<br>');
  return html;
}

/* ===================== 快捷键 & 分段控件 ===================== */
editor.addEventListener('keydown', e => {
  const isMod = e.metaKey || e.ctrlKey;
  if(isMod && e.key === 'Enter'){ e.preventDefault(); send(); }
});
sendBtn.addEventListener('click', send);

segBtns.forEach(b => b.addEventListener('click', async () => {
  segBtns.forEach(x => { x.classList.remove('active'); x.setAttribute('aria-selected','false'); });
  b.classList.add('active'); b.setAttribute('aria-selected','true');
  forcedType = b.dataset.type;
  config.last_type = forcedType;
  await saveConfig();
  refreshIntent();
}));

editor.addEventListener('input', refreshIntent);

/* ===================== 初始化 ===================== */
async function init(){
  await loadConfig();
  // 分段控件状态
  segBtns.forEach(b => b.classList.toggle('active', b.dataset.type === forcedType));
  renderBotMenu();
  updateBotPicker();
  renderAttachments();
  renderHistory();
  refreshIntent();
  setResult(null, '就绪');
  setTimeout(() => { const s = resultEl.querySelector('span:nth-child(2)'); if(s) s.innerHTML = '等待发送'; }, 100);
}
init();

})();
