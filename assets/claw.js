(() => {
  'use strict';

  const transcript = document.getElementById('transcript');
  const currentChat = transcript ? transcript.dataset.chat : null;
  const status = document.getElementById('chat-status');

  const scrollToBottom = () => {
    if (transcript) transcript.scrollTop = transcript.scrollHeight;
  };

  const markUnread = (chat) => {
    const dot = document.querySelector(`.chat-link[data-chat="${chat}"] .chat-dot`);
    if (dot) dot.classList.add('chat-dot--unread');
  };

  const onMessage = (event) => {
    const { chat, html } = JSON.parse(event.data);
    if (chat !== currentChat) {
      markUnread(chat);
      return;
    }
    if (transcript) {
      const pinned =
        transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 80;
      // Keep the activity indicator (if any) pinned to the very bottom.
      const typing = document.getElementById('typing');
      if (typing) typing.insertAdjacentHTML('beforebegin', html);
      else transcript.insertAdjacentHTML('beforeend', html);
      if (pinned) scrollToBottom();
    }
  };

  const showTyping = (detail) => {
    if (!transcript) return;
    let row = document.getElementById('typing');
    if (!row) {
      const pinned =
        transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 80;
      row = document.createElement('div');
      row.id = 'typing';
      row.className = 'typing';
      row.innerHTML =
        '<span class="typing-text"></span>' +
        '<span class="typing-dots"><i></i><i></i><i></i></span>';
      transcript.appendChild(row);
      if (pinned) scrollToBottom();
    }
    row.querySelector('.typing-text').textContent = detail || 'working';
  };

  const hideTyping = () => {
    const row = document.getElementById('typing');
    if (row) row.remove();
  };

  const onMessageUpdate = (event) => {
    const { html, id } = JSON.parse(event.data);
    const existing = document.getElementById(`m${id}`);
    if (existing) existing.outerHTML = html;
  };

  const onRun = (event) => {
    const { chat, state, detail } = JSON.parse(event.data);
    if (chat !== currentChat) return;
    if (state === 'idle') {
      if (status) {
        status.textContent = '';
        status.classList.remove('chat-status--working');
      }
      hideTyping();
      return;
    }
    if (status) {
      status.textContent = detail || 'working…';
      status.classList.add('chat-status--working');
    }
    showTyping(detail);
  };

  const connect = () => {
    const source = new EventSource('/events');
    source.addEventListener('message', onMessage);
    source.addEventListener('message_update', onMessageUpdate);
    source.addEventListener('run', onRun);
    source.onerror = () => {
      source.close();
      setTimeout(connect, 2000);
    };
  };

  // Question cards: clicking an option posts the answer. The collapsed card
  // arrives back over SSE as a message_update, so we only disable the buttons
  // optimistically and let the server drive the final render.
  if (transcript) {
    transcript.addEventListener('click', async (event) => {
      const button = event.target.closest('.qcard-option');
      if (!button) return;
      const { question, approval, option } = button.dataset;
      const card = button.closest('.msg');
      if (card) card.classList.add('qcard--pending');
      const url = approval
        ? `/api/approvals/${approval}/answer`
        : `/api/questions/${question}/answer`;
      const body = approval ? { decision: option } : { option };
      const response = await fetch(url, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
      });
      if (!response.ok && card) card.classList.remove('qcard--pending');
    });
  }

  // Archive / unarchive the current chat, then return to the chat list.
  const archiveButton = document.querySelector('.chat-archive');
  if (archiveButton) {
    archiveButton.addEventListener('click', async () => {
      const { chat, archived } = archiveButton.dataset;
      archiveButton.disabled = true;
      const response = await fetch(`/api/chats/${chat}/archive`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ archived: archived === 'true' }),
      });
      window.location.href = response.ok && archived === 'true' ? '/' : `/chats/${chat}`;
    });
  }

  const composer = document.getElementById('composer');
  const input = document.getElementById('composer-input');

  if (composer && input && currentChat) {
    composer.addEventListener('submit', async (event) => {
      event.preventDefault();
      const text = input.value.trim();
      if (!text) return;
      input.value = '';
      input.focus();
      const response = await fetch(`/api/chats/${currentChat}/messages`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ text }),
      });
      if (!response.ok) window.location.reload();
    });
    input.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && !event.shiftKey) {
        event.preventDefault();
        composer.requestSubmit();
      }
    });
  }

  // File browser (coder workspaces): a directory list on the left, a read-only
  // viewer on the right. Navigation and reads go through the jailed /files API;
  // names are server-supplied so they're escaped before they touch innerHTML.
  const fs = document.getElementById('fs');
  if (fs) {
    const chat = fs.dataset.chat;
    const listEl = document.getElementById('fs-list');
    const crumbEl = document.getElementById('fs-breadcrumb');
    const viewerEl = document.getElementById('fs-viewer');
    const msgEl = document.getElementById('fs-msg');
    const nameForm = document.getElementById('fs-newname');
    const nameInput = document.getElementById('fs-newname-input');
    const uploadInput = document.getElementById('fs-upload-input');
    let currentPath = '';
    let pending = null; // { mode: 'file'|'folder'|'rename', target }

    const escapeHtml = (text) =>
      text.replace(/[&<>"']/g, (ch) =>
        ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch]));
    const parentOf = (path) => (path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '');
    const join = (dir, name) => (dir ? `${dir}/${name}` : name);
    const rawUrl = (path) => `/chats/${chat}/files/raw?path=${encodeURIComponent(path)}`;
    const fmtSize = (n) => {
      if (n < 1024) return `${n} B`;
      if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} K`;
      return `${(n / (1024 * 1024)).toFixed(1)} M`;
    };
    const flash = (text) => {
      msgEl.textContent = text;
      if (text) setTimeout(() => { if (msgEl.textContent === text) msgEl.textContent = ''; }, 2500);
    };
    const api = (suffix, body) =>
      fetch(`/api/chats/${chat}/files${suffix}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
      });

    const renderCrumbs = (path) => {
      const crumbs = ['<a class="fs-crumb" data-path="">root</a>'];
      let acc = '';
      for (const part of path ? path.split('/') : []) {
        acc = join(acc, part);
        crumbs.push(`<a class="fs-crumb" data-path="${escapeHtml(acc)}">${escapeHtml(part)}</a>`);
      }
      crumbEl.innerHTML = crumbs.join('<span class="fs-sep">/</span>');
    };

    const navigate = async (path) => {
      const res = await fetch(`/api/chats/${chat}/files/list?path=${encodeURIComponent(path)}`);
      if (!res.ok) {
        listEl.innerHTML = '<li class="fs-error">cannot open this folder</li>';
        return;
      }
      currentPath = path;
      const { entries } = await res.json();
      renderCrumbs(path);
      const rows = [];
      if (path) {
        rows.push(`<li class="fs-row fs-row--up"><button class="fs-entry fs-up" data-dir="${escapeHtml(parentOf(path))}">..</button></li>`);
      }
      for (const entry of entries) {
        const child = escapeHtml(join(path, entry.name));
        const name = escapeHtml(entry.name);
        const open = entry.kind === 'dir'
          ? `<button class="fs-entry fs-dir" data-dir="${child}"><span class="fs-icon"></span><span class="fs-entry-name">${name}/</span></button>`
          : `<button class="fs-entry ${entry.kind === 'symlink' ? 'fs-symlink' : 'fs-file'}" data-file="${child}"><span class="fs-icon"></span><span class="fs-entry-name">${name}</span><span class="fs-size">${fmtSize(entry.size)}</span></button>`;
        rows.push(
          `<li class="fs-row">${open}<span class="fs-row-actions">` +
          `<button class="fs-act" data-act="rename" data-path="${child}" data-name="${name}" title="rename">✎</button>` +
          `<button class="fs-act" data-act="delete" data-path="${child}" title="delete">✕</button>` +
          '</span></li>',
        );
      }
      listEl.innerHTML = rows.join('') || '<li class="fs-hint">empty folder</li>';
    };

    const openFile = async (path) => {
      const res = await fetch(`/api/chats/${chat}/files/read?path=${encodeURIComponent(path)}`);
      const head =
        `<div class="fs-viewer-head"><span class="fs-filename">${escapeHtml(path)}</span>` +
        `<span class="fs-viewer-actions">SAVE<a class="fs-tool" href="${rawUrl(path)}">download</a></span></div>`;
      if (res.status === 415) {
        viewerEl.innerHTML = head.replace('SAVE', '') +
          '<p class="fs-hint">binary or too large to edit — download instead</p>';
        return;
      }
      if (!res.ok) {
        viewerEl.innerHTML = '<p class="fs-error">cannot read this file</p>';
        return;
      }
      const { content } = await res.json();
      viewerEl.innerHTML =
        head.replace('SAVE', '<button class="fs-tool" id="fs-save">save</button>') +
        '<textarea class="fs-editor" id="fs-editor" spellcheck="false"></textarea>';
      const editor = document.getElementById('fs-editor');
      editor.value = content;
      document.getElementById('fs-save').addEventListener('click', async () => {
        const saved = await api('/write', { path, content: editor.value });
        flash(saved.ok ? 'saved' : 'save failed');
      });
    };

    const promptName = (mode, prefill, target) => {
      pending = { mode, target };
      nameForm.hidden = false;
      nameInput.value = prefill || '';
      nameInput.focus();
      nameInput.select();
    };
    const hidePrompt = () => { nameForm.hidden = true; pending = null; nameInput.value = ''; };

    nameForm.addEventListener('submit', async (event) => {
      event.preventDefault();
      const name = nameInput.value.trim();
      if (!name || !pending) { hidePrompt(); return; }
      const { mode, target } = pending;
      let res;
      let toOpen = null;
      if (mode === 'folder') {
        res = await api('/mkdir', { path: join(currentPath, name) });
      } else if (mode === 'rename') {
        res = await api('/rename', { from: target, to: join(currentPath, name) });
      } else {
        toOpen = join(currentPath, name);
        res = await api('/write', { path: toOpen, content: '' });
      }
      hidePrompt();
      if (res.ok) {
        await navigate(currentPath);
        if (toOpen) openFile(toOpen);
      } else {
        flash(res.status === 409 ? 'already exists' : 'failed');
      }
    });
    nameInput.addEventListener('keydown', (event) => { if (event.key === 'Escape') hidePrompt(); });

    document.getElementById('fs-new-file').addEventListener('click', () => promptName('file'));
    document.getElementById('fs-new-folder').addEventListener('click', () => promptName('folder'));
    document.getElementById('fs-upload').addEventListener('click', () => uploadInput.click());
    uploadInput.addEventListener('change', async () => {
      const file = uploadInput.files[0];
      uploadInput.value = '';
      if (!file) return;
      const res = await fetch(
        `/api/chats/${chat}/files/upload?path=${encodeURIComponent(join(currentPath, file.name))}`,
        { method: 'POST', body: file },
      );
      if (res.ok) { await navigate(currentPath); flash(`uploaded ${file.name}`); }
      else flash('upload failed');
    });

    listEl.addEventListener('click', async (event) => {
      const act = event.target.closest('[data-act]');
      if (act) {
        const { act: kind, path } = act.dataset;
        if (kind === 'rename') {
          promptName('rename', act.dataset.name, path);
        } else if (kind === 'delete') {
          act.closest('.fs-row-actions').innerHTML =
            `<span class="fs-confirm">delete?</span>` +
            `<button class="fs-act fs-act--yes" data-act="delete-yes" data-path="${escapeHtml(path)}">yes</button>` +
            `<button class="fs-act" data-act="delete-no">no</button>`;
        } else if (kind === 'delete-yes') {
          const done = await api('/delete', { path });
          flash(done.ok ? 'deleted' : 'delete failed');
          navigate(currentPath);
        } else if (kind === 'delete-no') {
          navigate(currentPath);
        }
        return;
      }
      const dir = event.target.closest('[data-dir]');
      if (dir) { navigate(dir.dataset.dir); return; }
      const file = event.target.closest('[data-file]');
      if (file) {
        listEl.querySelectorAll('.fs-entry--active').forEach((el) => el.classList.remove('fs-entry--active'));
        file.classList.add('fs-entry--active');
        openFile(file.dataset.file);
      }
    });
    crumbEl.addEventListener('click', (event) => {
      const crumb = event.target.closest('[data-path]');
      if (crumb) navigate(crumb.dataset.path);
    });

    navigate('');
  }

  // Log viewer (admin → logs): renders the server snapshot, then live-appends new
  // records from the SSE stream. Level/target/search filters and pause/clear are
  // applied client-side over the in-DOM lines.
  const logView = document.getElementById('log-view');
  if (logView) {
    const levelSel = document.getElementById('log-level');
    const targetInput = document.getElementById('log-target');
    const searchInput = document.getElementById('log-search');
    const pauseBtn = document.getElementById('log-pause');
    const clearBtn = document.getElementById('log-clear');
    const RANK = { ERROR: 4, WARN: 3, INFO: 2, DEBUG: 1, TRACE: 0 };
    const MAX_LINES = 2000;
    let paused = false;

    const escapeHtml = (text) =>
      text.replace(/[&<>"']/g, (ch) =>
        ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch]));

    const matches = (line) => {
      const min = levelSel.value;
      if (min && (RANK[line.dataset.level] ?? 0) < (RANK[min] ?? 0)) return false;
      const target = targetInput.value.trim().toLowerCase();
      if (target && !line.dataset.target.toLowerCase().includes(target)) return false;
      const search = searchInput.value.trim().toLowerCase();
      if (search && !line.textContent.toLowerCase().includes(search)) return false;
      return true;
    };

    const applyFilters = () => {
      for (const line of logView.children) line.hidden = !matches(line);
    };

    const atBottom = () =>
      logView.scrollHeight - logView.scrollTop - logView.clientHeight < 40;

    const append = (record) => {
      const pinned = atBottom();
      const line = document.createElement('div');
      line.className = `log-line log-${record.level.toLowerCase()}`;
      line.dataset.level = record.level;
      line.dataset.target = record.target;
      const time = record.ts.substring(11, 19);
      line.innerHTML =
        `<span class="log-time">${time}</span>` +
        `<span class="log-level">${record.level}</span>` +
        `<span class="log-target">${escapeHtml(record.target)}</span>` +
        `<span class="log-msg">${escapeHtml(record.message)}</span>`;
      line.hidden = !matches(line);
      logView.appendChild(line);
      while (logView.childElementCount > MAX_LINES) logView.removeChild(logView.firstElementChild);
      if (pinned && !line.hidden) logView.scrollTop = logView.scrollHeight;
    };

    levelSel.addEventListener('change', applyFilters);
    targetInput.addEventListener('input', applyFilters);
    searchInput.addEventListener('input', applyFilters);
    clearBtn.addEventListener('click', () => { logView.replaceChildren(); });
    pauseBtn.addEventListener('click', () => {
      paused = !paused;
      pauseBtn.textContent = paused ? 'resume' : 'pause';
      pauseBtn.classList.toggle('log-paused', paused);
    });

    const connectLogs = () => {
      const source = new EventSource('/admin/logs/stream');
      source.addEventListener('log', (event) => {
        if (paused) return;
        append(JSON.parse(event.data));
      });
      source.onerror = () => { source.close(); setTimeout(connectLogs, 2000); };
    };

    applyFilters();
    logView.scrollTop = logView.scrollHeight;
    connectLogs();
  }

  connect();
  scrollToBottom();
})();
