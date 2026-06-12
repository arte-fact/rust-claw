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
      transcript.insertAdjacentHTML('beforeend', html);
      if (pinned) scrollToBottom();
    }
  };

  const onMessageUpdate = (event) => {
    const { html, id } = JSON.parse(event.data);
    const existing = document.getElementById(`m${id}`);
    if (existing) existing.outerHTML = html;
  };

  const onRun = (event) => {
    if (!status) return;
    const { chat, state, detail } = JSON.parse(event.data);
    if (chat !== currentChat || state === 'idle') {
      status.textContent = '';
      status.classList.remove('chat-status--working');
      return;
    }
    status.textContent = detail || 'working…';
    status.classList.add('chat-status--working');
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

    const escapeHtml = (text) =>
      text.replace(/[&<>"']/g, (ch) =>
        ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch]));
    const parentOf = (path) => (path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '');
    const join = (dir, name) => (dir ? `${dir}/${name}` : name);
    const fmtSize = (n) => {
      if (n < 1024) return `${n} B`;
      if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} K`;
      return `${(n / (1024 * 1024)).toFixed(1)} M`;
    };

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
      const url = `/api/chats/${chat}/files/list?path=${encodeURIComponent(path)}`;
      const res = await fetch(url);
      if (!res.ok) {
        listEl.innerHTML = '<li class="fs-error">cannot open this folder</li>';
        return;
      }
      const { entries } = await res.json();
      renderCrumbs(path);
      const rows = [];
      if (path) {
        rows.push(`<li><button class="fs-entry fs-up" data-dir="${escapeHtml(parentOf(path))}">..</button></li>`);
      }
      for (const entry of entries) {
        const child = escapeHtml(join(path, entry.name));
        const name = escapeHtml(entry.name);
        if (entry.kind === 'dir') {
          rows.push(`<li><button class="fs-entry fs-dir" data-dir="${child}"><span class="fs-icon"></span><span class="fs-entry-name">${name}/</span></button></li>`);
        } else {
          const kind = entry.kind === 'symlink' ? 'fs-symlink' : 'fs-file';
          rows.push(`<li><button class="fs-entry ${kind}" data-file="${child}"><span class="fs-icon"></span><span class="fs-entry-name">${name}</span><span class="fs-size">${fmtSize(entry.size)}</span></button></li>`);
        }
      }
      listEl.innerHTML = rows.join('') || '<li class="fs-hint">empty folder</li>';
    };

    const view = async (path) => {
      const url = `/api/chats/${chat}/files/read?path=${encodeURIComponent(path)}`;
      const res = await fetch(url);
      if (res.status === 415) {
        viewerEl.innerHTML = '<p class="fs-hint">can’t preview this file — it’s binary or too large</p>';
        return;
      }
      if (!res.ok) {
        viewerEl.innerHTML = '<p class="fs-error">cannot read this file</p>';
        return;
      }
      const { content } = await res.json();
      viewerEl.innerHTML = `<div class="fs-filename">${escapeHtml(path)}</div><pre class="fs-code"></pre>`;
      viewerEl.querySelector('.fs-code').textContent = content;
    };

    listEl.addEventListener('click', (event) => {
      const dir = event.target.closest('[data-dir]');
      if (dir) {
        navigate(dir.dataset.dir);
        return;
      }
      const file = event.target.closest('[data-file]');
      if (file) {
        listEl.querySelectorAll('.fs-entry--active').forEach((el) => el.classList.remove('fs-entry--active'));
        file.classList.add('fs-entry--active');
        view(file.dataset.file);
      }
    });
    crumbEl.addEventListener('click', (event) => {
      const crumb = event.target.closest('[data-path]');
      if (crumb) navigate(crumb.dataset.path);
    });

    navigate('');
  }

  connect();
  scrollToBottom();
})();
