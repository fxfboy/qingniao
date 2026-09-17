/* 青鸟 · 消息组装核心（纯函数，无 DOM 依赖）
 * 浏览器与 Node 双端加载：
 *  - 浏览器：index.html 以 <script> 引入，函数挂在 globalThis，main.js 直接调用
 *  - Node：scripts/capture-golden.mjs 以 ESM import 加载，读 globalThis.__qnMessage
 * 任何本文件行为变更都必须先更新 tests/golden/ 基线（见 Agent-CLI 方案 v3 §四 D12）
 */
(function (global) {
  'use strict';

  function detectType(text, chips) {
    var hasMd = /(\*\*[^*]+\*\*|`[^`]+`|\[[^\]]+\]\([^)]+\)|^#{1,3}\s|^[-*]\s|^>\s|^\d+\.\s)/m.test(text);
    var hasAt = /@所有人|@[^\s@]{1,12}/.test(text);
    if (/```card|\{\s*"config"|\{\s*"header"/.test(text))
      return { t: 'interactive', why: '检测到 <b>卡片 JSON</b>，将以交互卡片渲染' };
    if (chips && !text.trim())
      return { t: 'image', why: '仅包含 <b>' + chips + ' 张图片</b>，采用图片消息发送' };
    if (chips || hasMd || hasAt)
      return { t: 'post', why: '检测到 <b>' + [chips && (chips + ' 张图片'), hasMd && 'Markdown', hasAt && '@ 提及'].filter(Boolean).join('、') + '</b>，纯文本无法完整呈现' };
    if (!text.trim())
      return { t: 'text', why: '编辑器为空 · 输入内容后会自动判断消息类型' };
    return { t: 'text', why: '纯文字消息，直接以 text 类型发送' };
  }

  function parseInline(text) {
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

  function mdToPost(md, attImageKeys, titleOverride){
    const lines = md.split('\n');
    let title = titleOverride || '';
    const content = [];
    let inCodeBlock = false;
    let codeLines = [];
    for(const line of lines){
      if(line.trim().startsWith('```')){
        if(inCodeBlock){
          for(const cl of codeLines) content.push([{tag:'text', text: cl || ' '}]);
          codeLines = []; inCodeBlock = false;
        } else { inCodeBlock = true; }
        continue;
      }
      if(inCodeBlock){ codeLines.push(line); continue; }
      if(!title && line.startsWith('# ')){ title = line.slice(2).trim(); continue; }
      if(line.trim() === '') continue;
      let processed = line;
      if(processed.startsWith('> ')) processed = processed.slice(2);
      else if(processed.startsWith('>')) processed = processed.slice(1);
      const inline = parseInline(processed);
      if(inline.length) content.push(inline);
    }
    if(inCodeBlock && codeLines.length){
      for(const cl of codeLines) content.push([{tag:'text', text: cl || ' '}]);
    }
    for(const key of attImageKeys) content.push([{tag:'img', image_key:key}]);
    return {msg_type:'post', content:{post:{zh_cn:{title, content}}}};
  }

  function extractCardJson(text){
    let t = text.trim().replace(/^```card\s*/i,'').replace(/```\s*$/,'').trim();
    try { return JSON.parse(t); }
    catch(e) {
      const lines = text.trim().split('\n').filter(l => l.trim() !== '');
      const card = { config: {wide_screen_mode: true}, elements: [] };
      if(lines.length){
        const title = lines[0];
        const body = lines.slice(1).join('\n');
        card.header = { title: {tag: 'plain_text', content: title}, template: 'blue' };
        if(body) card.elements.push({tag: 'div', text: {tag: 'lark_md', content: body}});
      }
      return card;
    }
  }

  function buildPayload(text, type, imgKeys, title){
    switch(type){
      case 'text':  return {msg_type:'text', content:{text}};
      case 'post':  return mdToPost(text, imgKeys, title);
      case 'image':
        if(!imgKeys.length) throw new Error('没有可用的 image_key，请等待图片上传完成');
        return {msg_type:'image', content:{image_key:imgKeys[0]}};
      case 'interactive': {
        const card = extractCardJson(text);
        if(imgKeys.length && Array.isArray(card.elements)){
          for(const key of imgKeys){
            card.elements.push({tag: 'img', img_key: key, alt: {tag: 'plain_text', content: '图片'}});
          }
        }
        return {msg_type:'interactive', card};
      }
    }
  }

  async function hmacSign(secret, timestamp){
    const key = timestamp + '\n' + secret;
    const enc = new TextEncoder();
    const ck = await crypto.subtle.importKey('raw', enc.encode(key), {name:'HMAC', hash:'SHA-256'}, false, ['sign']);
    const sig = await crypto.subtle.sign('HMAC', ck, enc.encode(''));
    return btoa(String.fromCharCode(...new Uint8Array(sig)));
  }

  global.__qnMessage = {
    detectType: detectType,
    parseInline: parseInline,
    mdToPost: mdToPost,
    extractCardJson: extractCardJson,
    buildPayload: buildPayload,
    hmacSign: hmacSign,
  };
})(globalThis);
