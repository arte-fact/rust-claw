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

  connect();
  scrollToBottom();
})();
