/* CAYW preview bridge — runs inside the project page, injected only for ?cayw_edit=1. */
(() => {
  'use strict';

  const ID_ATTR = 'data-cayw-node';
  const ROOT_ID = 'cayw-editor-overlay';
  const STYLE_ID = 'cayw-editor-bridge-style';
  const SVG_NS = 'http://www.w3.org/2000/svg';
  const GENERATED_EMBEDS = new Set(['canvas', 'iframe', 'object', 'embed']);
  const MAX_TEXT = 4000;
  const MIN_SIZE = 16;
  const MAX_SIZE = 4000;
  const CONTAINER_TAGS = new Set([
    'body', 'main', 'header', 'footer', 'nav', 'section', 'article', 'aside',
    'div', 'figure', 'figcaption', 'form', 'fieldset', 'ul', 'ol', 'li', 'dl', 'dt', 'dd',
    'table', 'thead', 'tbody', 'tfoot', 'tr', 'td', 'th', 'details', 'blockquote', 'pre', 'address'
  ]);
  const TABLE_STRUCTURE_TAGS = new Set(['table', 'thead', 'tbody', 'tfoot', 'tr']);
  const PROTECTED_STRUCTURE = new Set(['html', 'head', 'body', 'main']);

  if (window.__caywBridgeLoaded) return;
  window.__caywBridgeLoaded = true;

  const script = document.currentScript ||
    document.querySelector('script[src*="__cayw_bridge.js"]');
  if (!script) return;

  let editorOrigin = '';
  try {
    const source = new URL(script.src, location.href);
    const raw = source.searchParams.get('editor_origin') || '';
    const parsed = new URL(raw);
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return;
    if (!['127.0.0.1', 'localhost', '[::1]', '::1'].includes(parsed.hostname) &&
        !/^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(parsed.hostname)) return;
    if (raw !== parsed.origin && raw !== `${parsed.origin}/`) return;
    editorOrigin = parsed.origin;
  } catch (error) {
    return;
  }

  if (window.parent === window) return;
  if (new URLSearchParams(location.search).get('cayw_edit') !== '1') return;

  const channel = new MessageChannel();
  let port = channel.port1;
  port.onmessage = (event) => onPortMessage(event.data);
  port.start();
  window.parent.postMessage({ type: 'cayw:connect', url: location.href }, editorOrigin, [channel.port2]);

  let editing = false;
  let root = null;
  let hoveredElement = null;
  let selectedElement = null;
  let selectedInfo = null;
  let hoverRaf = 0;
  let hoverPoint = null;
  let syncTimer = 0;
  let suppressClickUntil = 0;
  let dragState = null;
  let resizeState = null;
  let tipTimer = 0;

  const ui = {};

  const BRIDGE_CSS = `
    [data-cayw-ui]{
      position:fixed;inset:0;z-index:2147483000;pointer-events:none;
      font-family:-apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Hiragino Sans GB","Microsoft YaHei",sans-serif;
      font-size:12px;line-height:1.4;text-align:left;color:#1c2331;
    }
    [data-cayw-ui] *{box-sizing:border-box;}
    .cayw-hover-box{
      position:fixed;display:none;pointer-events:none;border:1px solid rgba(59,91,253,.9);
      background:rgba(59,91,253,.06);border-radius:2px;
    }
    .cayw-hover-box.cayw-readonly{
      border-style:dashed;border-color:rgba(113,123,144,.95);background:rgba(113,123,144,.06);
    }
    .cayw-ring{
      position:fixed;display:none;pointer-events:none;border:2px solid #3b5bfd;
      box-shadow:0 0 0 1px rgba(255,255,255,.85) inset,0 0 0 1px rgba(59,91,253,.45);
      border-radius:2px;
    }
    .cayw-ring.cayw-readonly{border-style:dashed;border-color:#8a93a6;box-shadow:none;}
    .cayw-size-tag{
      position:fixed;display:none;pointer-events:none;padding:2px 6px;border-radius:4px;
      background:#3b5bfd;color:#fff;font-size:11px;font-weight:600;white-space:nowrap;
      box-shadow:0 2px 8px rgba(0,0,0,.18);
    }
    .cayw-handle{
      position:fixed;display:none;width:11px;height:11px;pointer-events:auto;background:#fff;
      border:1.5px solid #3b5bfd;border-radius:2px;box-shadow:0 1px 3px rgba(0,0,0,.2);
    }
    .cayw-handle[data-dir="e"]{cursor:ew-resize;}
    .cayw-handle[data-dir="s"]{cursor:ns-resize;}
    .cayw-handle[data-dir="se"]{cursor:nwse-resize;}
    .cayw-line{
      position:fixed;display:none;pointer-events:none;background:#3b5bfd;border-radius:2px;
      box-shadow:0 0 0 1px #fff;
    }
    .cayw-drop{
      position:fixed;display:none;pointer-events:none;border:2px dashed #3b5bfd;
      background:rgba(59,91,253,.10);border-radius:4px;
    }
    .cayw-ghost{
      position:fixed;pointer-events:none;opacity:.55;overflow:hidden;background:#fff;
      border:1px solid #3b5bfd;box-shadow:0 10px 30px rgba(0,0,0,.22);border-radius:4px;
      z-index:2147483001;
    }
    .cayw-tip{
      position:fixed;display:none;pointer-events:none;max-width:280px;padding:6px 9px;
      border-radius:6px;background:#1c2331;color:#fff;font-size:12px;
      box-shadow:0 6px 18px rgba(0,0,0,.25);z-index:2147483002;transform:translate(-50%,-140%);
    }
    html.cayw-editing,html.cayw-editing body{cursor:default;}
    html.cayw-editing *{-webkit-user-select:none !important;user-select:none !important;}
    @media (prefers-reduced-motion: reduce){ .cayw-ghost{transition:none;} }
  `;

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', announce, { once: true });
  } else {
    announce();
  }

  /* ---------------------------------------------------------------- setup */

  function announce() {
    ensureUi();
    send({ type: 'cayw:ready' });
  }

  function send(message) {
    if (!port) return;
    try {
      port.postMessage(message);
    } catch (error) {
      // The parent may have navigated away; editing simply stops working.
    }
  }

  function ensureUi() {
    if (!document.getElementById(STYLE_ID)) {
      const style = document.createElement('style');
      style.id = STYLE_ID;
      style.textContent = BRIDGE_CSS;
      (document.head || document.documentElement).appendChild(style);
    }
    if (!root || !root.isConnected) {
      root = document.createElement('div');
      root.id = ROOT_ID;
      root.setAttribute('data-cayw-ui', '');
      document.documentElement.appendChild(root);
      ui.hover = null;
      ui.ring = null;
    }
    if (!ui.hover) {
      ui.hover = element('div', 'cayw-hover-box');
      ui.ring = element('div', 'cayw-ring');
      ui.sizeTag = element('div', 'cayw-size-tag');
      ui.line = element('div', 'cayw-line');
      ui.drop = element('div', 'cayw-drop');
      ui.tip = element('div', 'cayw-tip');
      ui.handles = ['e', 's', 'se'].map((dir) => {
        const handle = element('div', 'cayw-handle');
        handle.dataset.dir = dir;
        handle.addEventListener('pointerdown', (event) => onHandleDown(event, dir));
        return handle;
      });
    }
  }

  function element(tag, className) {
    const node = document.createElement(tag);
    node.className = className;
    root.appendChild(node);
    return node;
  }

  /* ------------------------------------------------------------ messaging */

  function onPortMessage(data) {
    if (!data || typeof data !== 'object' || typeof data.type !== 'string') return;
    if (data.type === 'cayw:mode' && typeof data.editing === 'boolean') {
      setEditing(data.editing);
      return;
    }
    if (!editing) return;
    if (data.type === 'cayw:palette') {
      handlePaletteMessage(data);
      return;
    }
    if (data.type === 'cayw:select' && typeof data.id === 'string') {
      if (!data.id) {
        clearSelection();
        return;
      }
      selectById(data.id, { reveal: true });
      return;
    }
    if (data.type === 'cayw:parent') {
      selectMappedParent();
    }
  }

  function setEditing(value) {
    editing = Boolean(value);
    document.documentElement.classList.toggle('cayw-editing', editing);
    ensureUi();
    if (!editing) {
      hideHover();
      clearSelection();
      hideDropFeedback();
      suppressClickUntil = 0;
    }
  }

  /* ------------------------------------------------------------ inspection */

  function tagOf(elementNode) {
    if (!elementNode || !elementNode.tagName) return '';
    return elementNode.tagName.toLowerCase();
  }

  function mappedInfo(elementNode, clicked) {
    const id = elementNode.getAttribute(ID_ATTR) || '';
    const duplicate = id ? document.querySelectorAll(`[${ID_ATTR}="${cssEscape(id)}"]`).length > 1 : false;
    const readonly = elementNode.getAttribute('data-cayw-readonly') === 'true';
    const dynamic = !id || duplicate || readonly;
    let reason = '';
    if (!id) reason = '该元素缺少源码标识。';
    else if (duplicate) reason = '源码标识在同页面重复，无法确定唯一目标。';
    else if (readonly) reason = elementNode.getAttribute('data-cayw-reason') || '该源码区域被标记为只读。';
    const partial = !dynamic && clicked !== elementNode;
    return { element: elementNode, id, dynamic, partial, reason };
  }

  function unmappedInfo(elementNode, reason) {
    return {
      element: elementNode,
      id: '',
      dynamic: true,
      partial: false,
      reason
    };
  }

  function svgInfo(elementNode) {
    const tag = tagOf(elementNode);
    if (tag === 'svg' && elementNode.hasAttribute(ID_ATTR)) {
      const info = mappedInfo(elementNode, elementNode);
      const parentSvg = elementNode.parentElement && elementNode.parentElement.closest
        ? elementNode.parentElement.closest('svg')
        : null;
      if (parentSvg) info.partial = true;
      return info;
    }
    let svg = elementNode.parentElement;
    while (svg && svg.namespaceURI === SVG_NS && tagOf(svg) !== 'svg') {
      svg = svg.parentElement;
    }
    if (svg && svg.namespaceURI === SVG_NS && tagOf(svg) === 'svg' && svg.hasAttribute(ID_ATTR)) {
      return mappedInfo(svg, elementNode);
    }
    return unmappedInfo(
      elementNode,
      'SVG 内部内容由 SVG 图形生成，不能直接改写；请通过“选择最近的源码容器”定位带源码标识的 SVG。'
    );
  }

  // A click only maps to the clicked element itself. Unmapped content stays
  // read-only; selecting an ID parent is an explicit user action (parent button).
  function infoFor(target) {
    if (!target) return null;
    const elementNode = target.nodeType === 3 ? target.parentElement : target;
    if (!elementNode || elementNode.nodeType !== 1) return null;
    if (elementNode.closest && elementNode.closest(`#${ROOT_ID}`)) return null;

    const rootNode = elementNode.getRootNode ? elementNode.getRootNode() : null;
    if (rootNode && rootNode.nodeType === 11 && rootNode.host) {
      return unmappedInfo(elementNode, 'Shadow DOM 内容不在页面源码中，不能直接编辑。');
    }
    if (elementNode.shadowRoot) {
      return unmappedInfo(elementNode, '该元素使用 Shadow DOM，内部内容不在页面源码中，不能直接编辑。');
    }

    const tag = tagOf(elementNode);
    if (GENERATED_EMBEDS.has(tag)) {
      return unmappedInfo(
        elementNode,
        `${tag} 的内容由浏览器或外部文档生成，没有唯一的源码子节点；请选择相邻的源码元素。`
      );
    }

    if (elementNode.namespaceURI === SVG_NS) return svgInfo(elementNode);

    if (elementNode.hasAttribute(ID_ATTR)) return mappedInfo(elementNode, elementNode);
    return unmappedInfo(
      elementNode,
      '该元素没有源码标识。脚本生成的内容不能直接编辑；请通过“选择最近的源码容器”定位父级。'
    );
  }

  function selectById(id, options = {}) {
    const matches = document.querySelectorAll(`[${ID_ATTR}="${cssEscape(id)}"]`);
    if (!matches.length) return false;
    const elementNode = matches[0];
    if (options.reveal) {
      try {
        elementNode.scrollIntoView({ block: 'nearest', inline: 'nearest' });
      } catch (error) {
        // Older engines may not accept the options object.
      }
    }
    const info = infoFor(elementNode);
    if (!info) return false;
    selectInfo(info);
    return true;
  }

  function selectInfo(info) {
    if (!info || !info.element || !info.element.isConnected) return;
    ensureUi();
    selectedInfo = info;
    selectedElement = info.element;
    drawSelection(info);
    hideHover();
    send(describe(info, 'cayw:select'));
  }

  function clearSelection() {
    selectedElement = null;
    selectedInfo = null;
    if (!ui.ring) return;
    ui.ring.style.display = 'none';
    ui.sizeTag.style.display = 'none';
    for (const handle of ui.handles) handle.style.display = 'none';
  }

  function describe(info, type) {
    const elementNode = info.element;
    const rect = elementNode.getBoundingClientRect();
    const style = getComputedStyle(elementNode);
    return {
      type,
      id: info.dynamic ? '' : info.id,
      tag: tagOf(elementNode),
      rect: {
        x: round(rect.left),
        y: round(rect.top),
        width: round(Math.max(0, rect.width)),
        height: round(Math.max(0, rect.height))
      },
      computed: {
        color: style.color || '',
        backgroundColor: style.backgroundColor || '',
        borderColor: style.borderTopColor || style.borderColor || '',
        width: style.width || '',
        height: style.height || '',
        fontFamily: style.fontFamily || '',
        fontSize: style.fontSize || '',
        fontWeight: style.fontWeight || '',
        lineHeight: style.lineHeight || '',
        textAlign: style.textAlign || '',
        display: style.display || '',
        gap: style.gap || '',
        flexDirection: style.flexDirection || '',
        gridTemplateColumns: String(style.gridTemplateColumns || '').slice(0, 400),
        alignItems: style.alignItems || '',
        justifyContent: style.justifyContent || ''
      },
      text: visibleText(elementNode),
      attributes: {
        src: elementNode.getAttribute('src') || '',
        href: elementNode.getAttribute('href') || ''
      },
      dynamic: Boolean(info.dynamic),
      partial: Boolean(info.partial),
      reason: info.reason || ''
    };
  }

  function visibleText(elementNode) {
    let text = '';
    if (typeof elementNode.innerText === 'string') text = elementNode.innerText;
    else text = elementNode.textContent || '';
    return text.replace(/\s+/g, ' ').trim().slice(0, MAX_TEXT);
  }

  /* --------------------------------------------------------------- visuals */

  function drawSelection(info) {
    ensureUi();
    const rect = info.element.getBoundingClientRect();
    ui.ring.className = `cayw-ring${info.dynamic ? ' cayw-readonly' : ''}`;
    place(ui.ring, rect);
    ui.ring.style.display = 'block';
    if (info.dynamic || !editing) {
      ui.sizeTag.style.display = 'none';
      for (const handle of ui.handles) handle.style.display = 'none';
      return;
    }
    placeHandles(rect);
    placeSizeTag(rect);
  }

  function placeHandles(rect) {
    const positions = {
      e: [rect.left + rect.width, rect.top + rect.height / 2],
      s: [rect.left + rect.width / 2, rect.top + rect.height],
      se: [rect.left + rect.width, rect.top + rect.height]
    };
    for (const handle of ui.handles) {
      const point = positions[handle.dataset.dir];
      handle.style.left = `${point[0]}px`;
      handle.style.top = `${point[1]}px`;
      handle.style.transform = 'translate(-50%, -50%)';
      handle.style.display = 'block';
    }
  }

  function placeSizeTag(rect) {
    const text = `${Math.round(rect.width)} × ${Math.round(rect.height)}`;
    ui.sizeTag.textContent = text;
    ui.sizeTag.style.display = 'block';
    const height = ui.sizeTag.offsetHeight || 20;
    let top = rect.top + rect.height + 6;
    if (top + height > window.innerHeight - 4) top = rect.top - height - 6;
    let left = rect.left;
    left = Math.max(4, Math.min(left, window.innerWidth - (ui.sizeTag.offsetWidth || 80) - 4));
    ui.sizeTag.style.left = `${left}px`;
    ui.sizeTag.style.top = `${Math.max(4, top)}px`;
  }

  function place(node, rect) {
    node.style.left = `${rect.left}px`;
    node.style.top = `${rect.top}px`;
    node.style.width = `${Math.max(0, rect.width)}px`;
    node.style.height = `${Math.max(0, rect.height)}px`;
  }

  function hideHover() {
    hoveredElement = null;
    if (ui.hover) ui.hover.style.display = 'none';
  }

  function hideDropFeedback() {
    if (ui.line) ui.line.style.display = 'none';
    if (ui.drop) ui.drop.style.display = 'none';
  }

  function showTip(text, x, y) {
    if (!text) return;
    ensureUi();
    ui.tip.textContent = text;
    ui.tip.style.display = 'block';
    ui.tip.style.left = `${Math.max(60, Math.min(x, window.innerWidth - 60))}px`;
    ui.tip.style.top = `${Math.max(40, y)}px`;
    clearTimeout(tipTimer);
    tipTimer = setTimeout(() => {
      ui.tip.style.display = 'none';
    }, 1600);
  }

  function scheduleSync() {
    if (!editing || !selectedElement || !selectedElement.isConnected) return;
    if (syncTimer) return;
    syncTimer = setTimeout(() => {
      syncTimer = 0;
      if (!editing || !selectedElement || !selectedElement.isConnected) return;
      const info = infoFor(selectedElement);
      if (!info) {
        clearSelection();
        return;
      }
      if (selectedInfo && selectedInfo.partial && !info.dynamic && info.id === selectedInfo.id) {
        info.partial = true;
      }
      selectedInfo = info;
      drawSelection(info);
      send(describe(info, 'cayw:select'));
    }, 80);
  }

  /* -------------------------------------------------------------- hover UX */

  document.addEventListener('pointermove', (event) => {
    if (!event.isTrusted || !editing || dragState || resizeState) return;
    if (event.target && event.target.closest && event.target.closest(`#${ROOT_ID}`)) {
      hideHover();
      return;
    }
    hoverPoint = { x: event.clientX, y: event.clientY };
    if (hoverRaf) return;
    hoverRaf = requestAnimationFrame(() => {
      hoverRaf = 0;
      if (!editing || dragState || resizeState || !hoverPoint) return;
      updateHover(hoverPoint.x, hoverPoint.y);
    });
  }, true);

  function updateHover(x, y) {
    const info = infoFor(document.elementFromPoint(x, y));
    if (!info) {
      hideHover();
      return;
    }
    if (selectedElement && info.element === selectedElement) {
      hideHover();
      return;
    }
    if (hoveredElement === info.element && ui.hover.dataset.dynamic === String(Boolean(info.dynamic || info.partial))) {
      return;
    }
    hoveredElement = info.element;
    ui.hover.className = `cayw-hover-box${info.dynamic || info.partial ? ' cayw-readonly' : ''}`;
    ui.hover.dataset.dynamic = String(Boolean(info.dynamic || info.partial));
    place(ui.hover, info.element.getBoundingClientRect());
    ui.hover.style.display = 'block';
  }

  document.addEventListener('scroll', () => {
    hideHover();
    scheduleSync();
  }, true);

  window.addEventListener('resize', scheduleSync);

  /* -------------------------------------------------------------- selection */

  document.addEventListener('click', (event) => {
    if (!event.isTrusted || !editing) return;
    if (event.target && event.target.closest && event.target.closest(`#${ROOT_ID}`)) return;
    if (Date.now() < suppressClickUntil) {
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    const info = infoFor(event.target);
    if (!info) return;
    event.preventDefault();
    event.stopPropagation();
    selectInfo(info);
  }, true);

  document.addEventListener('contextmenu', (event) => {
    if (!event.isTrusted || !editing) return;
    if (event.target && event.target.closest && event.target.closest(`#${ROOT_ID}`)) return;
    const info = infoFor(event.target);
    if (!info) return;
    event.preventDefault();
    event.stopPropagation();
    selectInfo(info);
    const message = describe(info, 'cayw:context');
    message.x = event.clientX;
    message.y = event.clientY;
    send(message);
  }, true);

  document.addEventListener('mousedown', (event) => {
    if (!event.isTrusted || !editing) return;
    if (event.target && event.target.closest && event.target.closest(`#${ROOT_ID}`)) return;
    event.preventDefault();
  }, true);

  document.addEventListener('submit', (event) => {
    if (!event.isTrusted || !editing) return;
    event.preventDefault();
    event.stopPropagation();
  }, true);

  /* ---------------------------------------------------------------- dragging */

  document.addEventListener('pointerdown', (event) => {
    if (!event.isTrusted || !editing || event.button !== 0) return;
    if (event.target && event.target.closest && event.target.closest(`#${ROOT_ID}`)) return;
    ensureUi();
    const info = infoFor(event.target);
    if (!info) return;
    selectInfo(info);
    if (info.dynamic || info.partial) return;
    if (PROTECTED_STRUCTURE.has(tagOf(info.element))) return;
    if (Date.now() < suppressClickUntil) return;
    const rect = info.element.getBoundingClientRect();
    const computed = getComputedStyle(info.element);
    const positionMode = computed.position === 'absolute' || computed.position === 'fixed'
      ? computed.position
      : '';
    dragState = {
      startX: event.clientX,
      startY: event.clientY,
      x: event.clientX,
      y: event.clientY,
      pointerId: event.pointerId,
      info,
      id: info.id,
      element: info.element,
      rect,
      grabX: event.clientX - rect.left,
      grabY: event.clientY - rect.top,
      positionMode,
      positionBase: positionMode ? positionedBase(info.element, positionMode) : null,
      captureTarget: info.element,
      lostCaptureHandler: null,
      leaveHandler: null,
      active: false,
      ghost: null,
      drop: null
    };
    try { info.element.setPointerCapture(event.pointerId); } catch (error) { /* ignore */ }
    const lostCapture = (lostEvent) => {
      if (lostEvent.isTrusted && dragState) onDragCancel(lostEvent);
    };
    dragState.lostCaptureHandler = lostCapture;
    info.element.addEventListener('lostpointercapture', lostCapture, { once: true });
    const leaveDocument = (leaveEvent) => {
      if (leaveEvent.isTrusted && !leaveEvent.relatedTarget && dragState) onDragCancel(leaveEvent);
    };
    dragState.leaveHandler = leaveDocument;
    document.addEventListener('pointerout', leaveDocument, true);
    event.preventDefault();
    document.addEventListener('pointermove', onDragMove, true);
    document.addEventListener('pointerup', onDragEnd, true);
    document.addEventListener('pointercancel', onDragCancel, true);
  }, true);

  function positionedBase(elementNode, mode) {
    const style = getComputedStyle(elementNode);
    const rect = elementNode.getBoundingClientRect();
    const pixelValue = (value) => {
      const match = String(value || '').trim().match(/^(-?\d+(?:\.\d+)?)px$/i);
      return match ? Number(match[1]) : null;
    };
    let left = pixelValue(style.left);
    let top = pixelValue(style.top);
    if (left === null || top === null) {
      if (mode === 'absolute') {
        left = elementNode.offsetLeft;
        top = elementNode.offsetTop;
      } else {
        left = rect.left;
        top = rect.top;
      }
    }
    let scaleX = 1;
    let scaleY = 1;
    const zoom = Number.parseFloat(style.zoom);
    if (Number.isFinite(zoom) && zoom > 0) {
      scaleX *= zoom;
      scaleY *= zoom;
    }
    if (style.transform && style.transform !== 'none' && typeof DOMMatrixReadOnly === 'function') {
      try {
        const matrix = new DOMMatrixReadOnly(style.transform);
        scaleX *= Math.hypot(matrix.a, matrix.b) || 1;
        scaleY *= Math.hypot(matrix.c, matrix.d) || 1;
      } catch (error) { /* keep unadjusted scale */ }
    }
    return {
      left: Number.isFinite(left) ? left : 0,
      top: Number.isFinite(top) ? top : 0,
      scaleX: Math.max(0.05, scaleX),
      scaleY: Math.max(0.05, scaleY)
    };
  }

  function onDragMove(event) {
    if (!event.isTrusted || !dragState) return;
    dragState.x = event.clientX;
    dragState.y = event.clientY;
    if (!dragState.active) {
      if (Math.hypot(event.clientX - dragState.startX, event.clientY - dragState.startY) < 6) return;
      dragState.active = true;
      beginDragVisual();
    }
    event.preventDefault();
    updateDragVisual(event.clientX, event.clientY);
  }

  function beginDragVisual() {
    hideHover();
    ui.ring.style.display = 'none';
    ui.sizeTag.style.display = 'none';
    for (const handle of ui.handles) handle.style.display = 'none';
    const clone = dragState.element.cloneNode(true);
    clone.querySelectorAll(`[${ID_ATTR}]`).forEach((node) => node.removeAttribute(ID_ATTR));
    clone.querySelectorAll('[id]').forEach((node) => node.removeAttribute('id'));
    clone.removeAttribute(ID_ATTR);
    clone.removeAttribute('id');
    clone.className = 'cayw-ghost';
    clone.style.position = 'fixed';
    clone.style.margin = '0';
    clone.style.width = `${dragState.rect.width}px`;
    clone.style.height = `${dragState.rect.height}px`;
    root.appendChild(clone);
    dragState.ghost = clone;
  }

  function updateDragVisual(x, y) {
    if (!dragState || !dragState.ghost) return;
    dragState.ghost.style.left = `${x - dragState.grabX}px`;
    dragState.ghost.style.top = `${y - dragState.grabY}px`;
    if (dragState.positionMode) {
      dragState.drop = null;
      hideDropFeedback();
      return;
    }
    const drop = computeDrop(x, y, dragState);
    dragState.drop = drop;
    if (drop && !drop.invalid) showDropFeedback(drop);
    else hideDropFeedback();
  }

  function computeDrop(x, y, state) {
    const under = document.elementFromPoint(x, y);
    if (!under) return null;
    const info = infoFor(under);
    if (!info) return null;
    if (info.dynamic) {
      return { invalid: true, reason: '目标区域不可编辑。', element: info.element };
    }
    if (info.partial) {
      return { invalid: true, reason: '目标包含未映射内容，不能执行结构操作。', element: info.element };
    }
    if (state.element && (info.element === state.element || state.element.contains(info.element))) {
      return { invalid: true, reason: '不能把元素放入自身或它的子元素。', element: info.element };
    }
    const rect = info.element.getBoundingClientRect();
    if (rect.width < 1 || rect.height < 1) return null;
    const tag = tagOf(info.element);
    const parent = info.element.parentElement;
    const parentTag = parent ? tagOf(parent) : '页面根';
    const parentContainer = parent && CONTAINER_TAGS.has(parentTag);
    const childTag = state.element ? tagOf(state.element) : '';
    const rowFlow = isRowFlow(parent);
    const position = rowFlow
      ? (x - rect.left) / Math.max(rect.width, 1)
      : (y - rect.top) / Math.max(rect.height, 1);
    const canInside = CONTAINER_TAGS.has(tag);
    let placement;
    if (canInside && position >= 0.28 && position <= 0.72) placement = 'inside';
    else placement = position < 0.5 ? 'before' : 'after';
    if (placement !== 'inside' && !parentContainer) {
      return {
        invalid: true,
        reason: `父级 <${parentTag}> 不是可接收同级内容的容器。`,
        element: info.element
      };
    }
    const tableParent = placement === 'inside' ? tag : parentTag;
    if (!tableChildAllowed(tableParent, childTag)) {
      return {
        invalid: true,
        reason: `<${tableParent}> 不能直接接收${childTag ? ` <${childTag}>` : '通用组件'}；请选择 td、th 或其他容器。`,
        element: info.element
      };
    }
    if (state.element && PROTECTED_STRUCTURE.has(tag) && placement !== 'inside') {
      return {
        invalid: true,
        reason: `<${tag}> 只允许作为“移入内部”的目标，不能在其前后插入。`,
        element: info.element
      };
    }
    if (placement === 'inside' && state.element && info.element === state.element.parentElement) {
      return { invalid: true, reason: '元素已经在这个容器内。', element: info.element };
    }
    return {
      element: info.element,
      info,
      rect,
      placement,
      tag
    };
  }

  function handlePaletteMessage(data) {
    if (data.phase === 'cancel') {
      hideDropFeedback();
      return;
    }
    const x = Number(data.x);
    const y = Number(data.y);
    if (!Number.isFinite(x) || !Number.isFinite(y)) return;
    if (x < 0 || y < 0 || x > window.innerWidth || y > window.innerHeight) {
      hideDropFeedback();
      return;
    }
    if (data.phase === 'move') {
      const drop = computeDrop(x, y, { element: null });
      if (drop && !drop.invalid) showDropFeedback(drop);
      else hideDropFeedback();
      return;
    }
    if (data.phase !== 'drop') return;
    const drop = computeDrop(x, y, { element: null });
    hideDropFeedback();
    let target = '';
    let placement = '';
    let reason = '';
    if (!drop) reason = '落点没有对应的页面元素。';
    else if (drop.invalid) reason = drop.reason || '该位置不能插入组件。';
    else if (!drop.info || !drop.info.id) reason = '落点没有唯一的源码映射。';
    else if (drop.info.dynamic) reason = '目标区域只读，不能插入组件。';
    else {
      target = drop.info.id;
      placement = drop.placement;
      const label = placement === 'inside' ? '内部' : placement === 'before' ? '前面' : '后面';
      showTip(`插入到 ${drop.tag} ${label}`, x, y);
    }
    send({
      type: 'cayw:palette-result',
      request: data.request,
      target,
      placement,
      reason
    });
  }

  function showDropFeedback(drop) {
    if (drop.placement === 'inside') {
      ui.line.style.display = 'none';
      place(ui.drop, {
        left: drop.rect.left - 2,
        top: drop.rect.top - 2,
        width: drop.rect.width + 4,
        height: drop.rect.height + 4
      });
      ui.drop.style.display = 'block';
      return;
    }
    ui.drop.style.display = 'none';
    const parent = drop.element.parentElement;
    const rowFlow = isRowFlow(parent);
    const line = ui.line;
    if (rowFlow) {
      line.style.width = '3px';
      line.style.height = `${Math.max(6, drop.rect.height + 4)}px`;
      line.style.left = `${(drop.placement === 'before' ? drop.rect.left : drop.rect.left + drop.rect.width) - 1.5}px`;
      line.style.top = `${drop.rect.top - 2}px`;
    } else {
      line.style.width = `${Math.max(6, drop.rect.width + 4)}px`;
      line.style.height = '3px';
      line.style.left = `${drop.rect.left - 2}px`;
      line.style.top = `${(drop.placement === 'before' ? drop.rect.top : drop.rect.top + drop.rect.height) - 1.5}px`;
    }
    line.style.display = 'block';
  }

  function isRowFlow(elementNode) {
    if (!elementNode) return false;
    const style = getComputedStyle(elementNode);
    if (style.display === 'grid' || style.display === 'inline-grid') return true;
    if (style.display === 'flex' || style.display === 'inline-flex') {
      return String(style.flexDirection || 'row').startsWith('row');
    }
    return false;
  }

  function tableChildAllowed(parentTag, childTag) {
    if (!TABLE_STRUCTURE_TAGS.has(parentTag)) return true;
    if (!childTag) return false;
    if (parentTag === 'table') {
      return ['caption', 'colgroup', 'thead', 'tbody', 'tfoot', 'tr'].includes(childTag);
    }
    if (parentTag === 'thead' || parentTag === 'tbody' || parentTag === 'tfoot') {
      return childTag === 'tr';
    }
    return childTag === 'td' || childTag === 'th';
  }

  function onDragEnd(event) {
    if (!event.isTrusted || !dragState) return;
    const state = dragState;
    const drop = state.active && !state.positionMode
      ? computeDrop(event.clientX, event.clientY, state)
      : null;
    cleanupDrag();
    if (!state.active) return;
    suppressClickUntil = Date.now() + 420;
    if (state.positionMode && state.positionBase) {
      const dx = (event.clientX - state.startX) / state.positionBase.scaleX;
      const dy = (event.clientY - state.startY) / state.positionBase.scaleY;
      if (Math.abs(dx) < 0.5 && Math.abs(dy) < 0.5) {
        if (selectedInfo) drawSelection(selectedInfo);
        return;
      }
      send({
        type: 'cayw:operation',
        operation: {
          kind: 'style',
          id: state.id,
          properties: {
            left: `${round(state.positionBase.left + dx)}px`,
            top: `${round(state.positionBase.top + dy)}px`
          }
        }
      });
      return;
    }
    if (drop && drop.invalid) {
      showTip(drop.reason, event.clientX, event.clientY);
      if (selectedInfo) drawSelection(selectedInfo);
      return;
    }
    if (drop && drop.info && drop.info.id && !drop.info.dynamic && !drop.info.partial) {
      send({
        type: 'cayw:operation',
        operation: {
          kind: 'move',
          id: state.id,
          target: drop.info.id,
          placement: drop.placement
        }
      });
    } else if (selectedInfo) {
      drawSelection(selectedInfo);
    }
  }

  function onDragCancel(event) {
    if (!event || !event.isTrusted || !dragState) return;
    suppressClickUntil = Date.now() + 300;
    cleanupDrag();
    if (selectedInfo) drawSelection(selectedInfo);
  }

  function cleanupDrag() {
    const state = dragState;
    if (state && state.ghost) state.ghost.remove();
    if (state && state.captureTarget) {
      if (state.lostCaptureHandler) {
        state.captureTarget.removeEventListener('lostpointercapture', state.lostCaptureHandler);
      }
      try {
        if (state.captureTarget.hasPointerCapture(state.pointerId)) {
          state.captureTarget.releasePointerCapture(state.pointerId);
        }
      } catch (error) { /* ignore */ }
    }
    if (state && state.leaveHandler) {
      document.removeEventListener('pointerout', state.leaveHandler, true);
    }
    dragState = null;
    hideDropFeedback();
    document.removeEventListener('pointermove', onDragMove, true);
    document.removeEventListener('pointerup', onDragEnd, true);
    document.removeEventListener('pointercancel', onDragCancel, true);
  }

  /* ---------------------------------------------------------------- resizing */

  function onHandleDown(event, direction) {
    if (!event.isTrusted || !editing || event.button !== 0) return;
    if (!selectedInfo || selectedInfo.dynamic || !selectedElement) return;
    event.preventDefault();
    event.stopPropagation();
    const rect = selectedElement.getBoundingClientRect();
    const style = getComputedStyle(selectedElement);
    const boxSizing = style.boxSizing === 'border-box' ? 'border-box' : 'content-box';
    const extraWidth = boxSizing === 'content-box'
      ? cssPx(style.paddingLeft) + cssPx(style.paddingRight) +
        cssPx(style.borderLeftWidth) + cssPx(style.borderRightWidth)
      : 0;
    const extraHeight = boxSizing === 'content-box'
      ? cssPx(style.paddingTop) + cssPx(style.paddingBottom) +
        cssPx(style.borderTopWidth) + cssPx(style.borderBottomWidth)
      : 0;
    const handle = event.currentTarget;
    resizeState = {
      direction,
      startX: event.clientX,
      startY: event.clientY,
      pointerId: event.pointerId,
      captureTarget: handle,
      lostCaptureHandler: null,
      leaveHandler: null,
      rect,
      width: rect.width,
      height: rect.height,
      boxSizing,
      extraWidth,
      extraHeight,
      newWidth: Math.round(rect.width),
      newHeight: Math.round(rect.height),
      cssWidth: boxToCssSize(Math.round(rect.width), boxSizing, extraWidth),
      cssHeight: boxToCssSize(Math.round(rect.height), boxSizing, extraHeight),
      changed: false
    };
    try { handle.setPointerCapture(event.pointerId); } catch (error) { /* ignore */ }
    const lostCapture = (lostEvent) => {
      if (lostEvent.isTrusted && resizeState) onResizeCancel(lostEvent);
    };
    resizeState.lostCaptureHandler = lostCapture;
    handle.addEventListener('lostpointercapture', lostCapture, { once: true });
    const leaveDocument = (leaveEvent) => {
      if (leaveEvent.isTrusted && !leaveEvent.relatedTarget && resizeState) onResizeCancel(leaveEvent);
    };
    resizeState.leaveHandler = leaveDocument;
    document.addEventListener('pointerout', leaveDocument, true);
    document.addEventListener('pointermove', onResizeMove, true);
    document.addEventListener('pointerup', onResizeEnd, true);
    document.addEventListener('pointercancel', onResizeCancel, true);
  }

  function onResizeMove(event) {
    if (!event.isTrusted || !resizeState) return;
    event.preventDefault();
    const dx = event.clientX - resizeState.startX;
    const dy = event.clientY - resizeState.startY;
    let width = resizeState.width;
    let height = resizeState.height;
    if (resizeState.direction === 'e' || resizeState.direction === 'se') {
      width = clamp(resizeState.width + dx, MIN_SIZE, MAX_SIZE);
    }
    if (resizeState.direction === 's' || resizeState.direction === 'se') {
      height = clamp(resizeState.height + dy, MIN_SIZE, MAX_SIZE);
    }
    const cssWidth = boxToCssSize(width, resizeState.boxSizing, resizeState.extraWidth);
    const cssHeight = boxToCssSize(height, resizeState.boxSizing, resizeState.extraHeight);
    resizeState.cssWidth = cssWidth;
    resizeState.cssHeight = cssHeight;
    resizeState.newWidth = Math.round(cssWidth + resizeState.extraWidth);
    resizeState.newHeight = Math.round(cssHeight + resizeState.extraHeight);
    resizeState.changed = resizeState.newWidth !== Math.round(resizeState.width) ||
      resizeState.newHeight !== Math.round(resizeState.height);
    ensureUi();
    ui.ring.style.display = 'none';
    ui.sizeTag.style.display = 'none';
    place(ui.drop, {
      left: resizeState.rect.left,
      top: resizeState.rect.top,
      width: resizeState.newWidth,
      height: resizeState.newHeight
    });
    ui.drop.style.display = 'block';
    ui.sizeTag.textContent = `${resizeState.newWidth} × ${resizeState.newHeight}`;
    ui.sizeTag.style.display = 'block';
    ui.sizeTag.style.left = `${resizeState.rect.left}px`;
    ui.sizeTag.style.top = `${resizeState.rect.top + resizeState.newHeight + 6}px`;
  }

  function onResizeEnd(event) {
    if (!event.isTrusted || !resizeState) return;
    const state = resizeState;
    cleanupResize();
    suppressClickUntil = Date.now() + 320;
    if (!state.changed || !selectedInfo || selectedInfo.dynamic) {
      if (selectedInfo) drawSelection(selectedInfo);
      return;
    }
    const properties = {};
    if (state.newWidth !== Math.round(state.width)) properties.width = `${round(state.cssWidth)}px`;
    if (state.newHeight !== Math.round(state.height)) properties.height = `${round(state.cssHeight)}px`;
    if (!Object.keys(properties).length) {
      if (selectedInfo) drawSelection(selectedInfo);
      return;
    }
    if (selectedInfo) drawSelection(selectedInfo);
    send({
      type: 'cayw:operation',
      operation: { kind: 'style', id: selectedInfo.id, properties }
    });
  }

  function onResizeCancel(event) {
    if (!event || !event.isTrusted || !resizeState) return;
    cleanupResize();
    if (selectedInfo) drawSelection(selectedInfo);
  }

  function cleanupResize() {
    const state = resizeState;
    if (state && state.captureTarget) {
      if (state.lostCaptureHandler) {
        state.captureTarget.removeEventListener('lostpointercapture', state.lostCaptureHandler);
      }
      try {
        if (state.captureTarget.hasPointerCapture(state.pointerId)) {
          state.captureTarget.releasePointerCapture(state.pointerId);
        }
      } catch (error) { /* ignore */ }
    }
    if (state && state.leaveHandler) {
      document.removeEventListener('pointerout', state.leaveHandler, true);
    }
    resizeState = null;
    hideDropFeedback();
    if (ui.sizeTag) ui.sizeTag.style.display = selectedInfo && !selectedInfo.dynamic && editing ? 'block' : 'none';
    document.removeEventListener('pointermove', onResizeMove, true);
    document.removeEventListener('pointerup', onResizeEnd, true);
    document.removeEventListener('pointercancel', onResizeCancel, true);
  }

  /* ------------------------------------------------------------ shortcuts */

  document.addEventListener('keydown', (event) => {
    if (!event.isTrusted || !editing) return;
    const mod = event.metaKey || event.ctrlKey;
    if (!mod) return;
    if (event.key.toLowerCase() !== 'z') return;
    if (event.isComposing || isTypingTarget(event.target)) return;
    event.preventDefault();
    send({
      type: 'cayw:operation',
      operation: { kind: event.shiftKey ? 'redo' : 'undo' }
    });
  }, true);

  function isTypingTarget(target) {
    if (!target || target.nodeType !== 1) return false;
    const tag = target.tagName;
    return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || target.isContentEditable;
  }

  /* ---------------------------------------------------------------- utils */

  function selectMappedParent() {
    if (!selectedElement) return;
    let node = selectedElement.parentElement;
    if (!node) {
      const rootNode = selectedElement.getRootNode ? selectedElement.getRootNode() : null;
      if (rootNode && rootNode.host) node = rootNode.host;
    }
    while (node && !(node.hasAttribute && node.hasAttribute(ID_ATTR))) {
      node = node.parentElement;
    }
    if (!node) return;
    const info = infoFor(node);
    if (info) selectInfo(info);
  }

  function cssEscape(value) {
    if (window.CSS && typeof window.CSS.escape === 'function') return window.CSS.escape(value);
    return String(value).replace(/["\\\]\[]/g, '\\$&');
  }

  function round(value) {
    return Math.round(value * 10) / 10;
  }

  function clamp(value, min, max) {
    return Math.min(Math.max(value, min), Math.max(min, max));
  }

  function cssPx(value) {
    const number = Number.parseFloat(value);
    return Number.isFinite(number) ? number : 0;
  }

  function boxToCssSize(borderBox, boxSizing, extra) {
    const size = boxSizing === 'border-box'
      ? borderBox
      : Math.max(0, borderBox - extra);
    return round(size);
  }
})();
