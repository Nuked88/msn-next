<script lang="ts">
  import { invoke, isTauri } from '@tauri-apps/api/core'
  import { listen, type UnlistenFn } from '@tauri-apps/api/event'
  import { open } from '@tauri-apps/plugin-dialog'
  import QRCode from 'qrcode'
  import { onMount, tick } from 'svelte'
  import {
    Activity,
    ChevronRight,
    Copy,
    Link2,
    LockKeyhole,
    MessageCircleMore,
    Paperclip,
    Plus,
    Power,
    QrCode,
    Radio,
    Send,
    Settings2,
    ShieldCheck,
    Smile,
    Sparkles,
    UserRoundPlus,
    X,
    Zap,
  } from '@lucide/svelte'

  type Contact = {
    peerId: string
    name: string
    online: boolean
    secure: boolean
    unread: number
  }

  type ClientMessage = {
    peerId: string
    direction: 'in' | 'out'
    kind: string
    body: string
    timestampMs: number
  }

  type ChatMessage = {
    id: string
    kind: 'incoming' | 'outgoing' | 'nudge' | 'file'
    body: string
    time: string
    mine: boolean
  }

  type ClientEvent =
    | { type: 'started'; peerId: string; displayName: string }
    | { type: 'contactUpdated'; contact: Omit<Contact, 'unread'> }
    | { type: 'conversationLoaded'; peerId: string; messages: ClientMessage[] }
    | { type: 'message'; message: ClientMessage }
    | { type: 'contactLink'; link: string }
    | { type: 'attachmentReceived'; peerId: string; path: string }
    | { type: 'error'; message: string }
    | { type: 'ready' }
    | { type: 'stopped' }

  const emoji = ['🙂', '😂', '❤️', '😮', '😎', '👋', '✨', '😭']
  let displayName = localStorage.getItem('msnnext-name') || 'Amico'
  let directAddress = ''
  let searchQuery = ''
  let messageText = ''
  let contactLink = ''
  let ownContactLink = ''
  let ownContactQr = ''
  let peerId = ''
  let selectedPeerId = ''
  let contacts: Contact[] = []
  let conversations: Record<string, ChatMessage[]> = {}
  let running = false
  let starting = false
  let setupOpen = true
  let connectOpen = false
  let detailsOpen = true
  let emojiOpen = false
  let linkRequested = false
  let toastText = ''
  let toastTimer: ReturnType<typeof setTimeout>
  let messageList: HTMLDivElement

  $: activeContact = contacts.find((contact) => contact.peerId === selectedPeerId)
  $: messages = selectedPeerId ? conversations[selectedPeerId] || [] : []
  $: visibleContacts = contacts.filter((contact) =>
    contact.name.toLocaleLowerCase().includes(searchQuery.trim().toLocaleLowerCase())
  )
  $: ready = Boolean(activeContact?.online && activeContact?.secure)

  onMount(() => {
    if (!isTauri()) return
    let unlisten: UnlistenFn | undefined
    void listen<ClientEvent>('client-event', ({ payload }) => handleEvent(payload)).then((stop) => {
      unlisten = stop
    })
    void invoke<boolean>('node_status').then((isRunning) => {
      running = isRunning
      setupOpen = !isRunning
    })
    return () => unlisten?.()
  })

  function handleEvent(event: ClientEvent) {
    if (event.type === 'started') {
      peerId = event.peerId
      displayName = event.displayName
      running = true
      setupOpen = false
      return
    }
    if (event.type === 'contactUpdated') {
      upsertContact(event.contact)
      return
    }
    if (event.type === 'conversationLoaded') {
      conversations = {
        ...conversations,
        [event.peerId]: event.messages.map(toChatMessage),
      }
      scrollMessages()
      return
    }
    if (event.type === 'message') {
      addMessage(event.message)
      return
    }
    if (event.type === 'contactLink') {
      ownContactLink = event.link
      linkRequested = false
      void QRCode.toDataURL(event.link, {
        width: 220,
        margin: 1,
        color: { dark: '#121a2a', light: '#ffffff' },
      }).then((qr) => ownContactQr = qr)
      return
    }
    if (event.type === 'attachmentReceived') {
      showToast(`File ricevuto: ${event.path}`)
      return
    }
    if (event.type === 'error') {
      linkRequested = false
      showToast(event.message)
      return
    }
    if (event.type === 'ready') {
      if (contacts.length === 0) void openContacts()
      return
    }
    running = false
    contacts = contacts.map((contact) => ({ ...contact, online: false, secure: false }))
  }

  function upsertContact(next: Omit<Contact, 'unread'>) {
    const existing = contacts.find((contact) => contact.peerId === next.peerId)
    if (existing) {
      contacts = contacts.map((contact) =>
        contact.peerId === next.peerId ? { ...contact, ...next } : contact
      )
    } else {
      contacts = [...contacts, { ...next, unread: 0 }]
    }
    if (!selectedPeerId) selectedPeerId = next.peerId
  }

  function toChatMessage(message: ClientMessage): ChatMessage {
    return {
      id: `${message.timestampMs}-${crypto.randomUUID()}`,
      kind: message.kind === 'nudge'
        ? 'nudge'
        : message.kind === 'file'
          ? 'file'
          : message.direction === 'out'
            ? 'outgoing'
            : 'incoming',
      body: message.body,
      mine: message.direction === 'out',
      time: new Intl.DateTimeFormat('it', {
        hour: '2-digit',
        minute: '2-digit',
      }).format(new Date(message.timestampMs)),
    }
  }

  function addMessage(message: ClientMessage) {
    const conversation = conversations[message.peerId] || []
    conversations = {
      ...conversations,
      [message.peerId]: [...conversation, toChatMessage(message)],
    }
    if (message.direction === 'in' && selectedPeerId !== message.peerId) {
      contacts = contacts.map((contact) =>
        contact.peerId === message.peerId
          ? { ...contact, unread: contact.unread + 1 }
          : contact
      )
    }
    if (message.kind === 'nudge') shakeWindow()
    scrollMessages()
  }

  function selectContact(peer: string) {
    selectedPeerId = peer
    contacts = contacts.map((contact) =>
      contact.peerId === peer ? { ...contact, unread: 0 } : contact
    )
    scrollMessages()
  }

  function contactSubtitle(contact: Contact) {
    const last = conversations[contact.peerId]?.at(-1)
    if (last) return last.kind === 'nudge' ? '⚡ Trillo' : last.kind === 'file' ? `📎 ${last.body}` : last.body
    return contact.secure ? 'Canale sicuro' : contact.online ? 'Collegamento in corso…' : 'Offline'
  }

  function scrollMessages() {
    void tick().then(() =>
      messageList?.scrollTo({ top: messageList.scrollHeight, behavior: 'smooth' })
    )
  }

  async function startNode() {
    if (!displayName.trim()) return
    starting = true
    try {
      await invoke('node_start', {
        config: { name: displayName.trim(), connect: directAddress.trim() || null },
      })
      localStorage.setItem('msnnext-name', displayName.trim())
    } catch (error) {
      showToast(String(error))
    } finally {
      starting = false
    }
  }

  async function stopNode() {
    await invoke('node_stop')
    running = false
    contacts = contacts.map((contact) => ({ ...contact, online: false, secure: false }))
  }

  async function sendMessage() {
    const text = messageText.trim().replace(/\s*\n+\s*/g, ' ')
    if (!text || !ready || !selectedPeerId) return
    try {
      await invoke('node_send_text', { peerId: selectedPeerId, text })
      messageText = ''
      emojiOpen = false
    } catch (error) {
      showToast(String(error))
    }
  }

  async function sendNudge() {
    if (!ready || !selectedPeerId) return
    try {
      await invoke('node_send_nudge', { peerId: selectedPeerId })
    } catch (error) {
      showToast(String(error))
    }
  }

  async function chooseFile() {
    if (!ready || !selectedPeerId) return
    const selected = await open({ multiple: false, directory: false })
    if (!selected || Array.isArray(selected)) return
    try {
      await invoke('node_send_file', { peerId: selectedPeerId, path: selected })
    } catch (error) {
      showToast(String(error))
    }
  }

  async function importContact() {
    if (!contactLink.trim().startsWith('msnnext://add/')) {
      showToast('Il link contatto non è valido')
      return
    }
    try {
      await invoke('node_import_contact', { link: contactLink.trim() })
      contactLink = ''
      showToast('Contatto verificato. Provo a collegarlo…')
    } catch (error) {
      showToast(String(error))
    }
  }

  async function scanContactQr() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: 'Immagini QR', extensions: ['png', 'jpg', 'jpeg', 'webp', 'gif'] }],
    })
    if (!selected || Array.isArray(selected)) return
    try {
      contactLink = await invoke<string>('scan_contact_qr', { path: selected })
      await importContact()
    } catch (error) {
      showToast(String(error))
    }
  }

  async function createContactLink() {
    if (!running) return
    linkRequested = true
    try {
      await invoke('node_request_contact_link')
    } catch (error) {
      linkRequested = false
      showToast(String(error))
    }
  }

  async function openContacts() {
    connectOpen = true
    if (!ownContactLink && running) await createContactLink()
  }

  async function copyOwnLink() {
    if (!ownContactLink) return
    await navigator.clipboard.writeText(ownContactLink)
    showToast('Link contatto copiato')
  }

  function shakeWindow() {
    const frame = document.querySelector('.app-frame')
    frame?.classList.add('shake')
    window.setTimeout(() => frame?.classList.remove('shake'), 700)
  }

  function showToast(text: string) {
    toastText = text
    clearTimeout(toastTimer)
    toastTimer = setTimeout(() => toastText = '', 3200)
  }
</script>

<main class:details-open={detailsOpen} class="app-frame">
  <aside class="rail">
    <div class="brand" aria-label="msnnext">nxt<span>.</span></div>
    <nav aria-label="Navigazione">
      <button class="rail-action active" aria-label="Conversazioni"><MessageCircleMore size={20} /></button>
      <button class="rail-action" aria-label="Aggiungi contatto" onclick={openContacts}><UserRoundPlus size={20} /></button>
      <button class="rail-action" aria-label="Impostazioni" onclick={() => detailsOpen = !detailsOpen}><Settings2 size={20} /></button>
    </nav>
    <button class:online={running} class="power-button" aria-label={running ? 'Arresta nodo' : 'Avvia nodo'} onclick={running ? stopNode : () => setupOpen = true}>
      <Power size={18} />
    </button>
  </aside>

  <aside class="contacts-pane">
    <header class="pane-heading">
      <div>
        <span class="eyebrow">Messaggi</span>
        <h1>Conversazioni</h1>
      </div>
      <button class="round-button" aria-label="Aggiungi contatto" onclick={openContacts}><Plus size={18} /></button>
    </header>

    <label class="search-field">
      <span>Contatti</span>
      <input bind:value={searchQuery} aria-label="Cerca contatti" placeholder="Cerca" />
    </label>

    <section class="contact-list" aria-label="Contatti">
      {#if contacts.length}
        {#each visibleContacts as contact (contact.peerId)}
          <button class:active={contact.peerId === selectedPeerId} class="contact-row" onclick={() => selectContact(contact.peerId)}>
            <span class="avatar contact-avatar">{contact.name.slice(0, 1).toUpperCase()}</span>
            <span class="contact-copy">
              <strong>{contact.name}</strong>
              <small>{contactSubtitle(contact)}</small>
            </span>
            {#if contact.unread}<b class="unread">{contact.unread}</b>{:else}<i class:online={contact.online} class="presence"></i>{/if}
          </button>
        {/each}
      {:else}
        <div class="empty-contacts">
          <span class="empty-orbit"><Radio size={20} /></span>
          <strong>Qui appariranno i tuoi contatti</strong>
          <p>Condividi il tuo QR oppure incolla il link ricevuto da un amico.</p>
          <button onclick={openContacts}>Scopri come <ChevronRight size={15} /></button>
        </div>
      {/if}
    </section>

    <footer class="identity-card">
      <span class="avatar me">{displayName.slice(0, 1).toUpperCase()}</span>
      <span><strong>{displayName}</strong><small><i class:online={running}></i>{running ? 'Online' : 'Nodo spento'}</small></span>
    </footer>
  </aside>

  <section class="conversation">
    <header class="conversation-header">
      <div class="conversation-person">
        <span class="avatar large">{activeContact?.name.slice(0, 1).toUpperCase() || '?'}</span>
        <span>
          <strong>{activeContact?.name || 'Nuova conversazione'}</strong>
          <small>{ready ? 'Online · canale sicuro' : activeContact?.online ? 'Protezione del canale…' : activeContact ? 'Offline' : 'Aggiungi un contatto per iniziare'}</small>
        </span>
      </div>
      <div class="header-actions">
        <span class:secure={ready} class="security-badge"><ShieldCheck size={15} />{ready ? 'Protetta' : 'Non pronta'}</span>
        <button class="nudge-button" disabled={!ready} onclick={sendNudge}><Zap size={17} /> Trillo</button>
      </div>
    </header>

    <div class="messages" bind:this={messageList} aria-live="polite">
      {#if !activeContact}
        <div class="welcome">
          <div class="welcome-mark"><Sparkles size={30} /></div>
          <p class="eyebrow">Tre passaggi</p>
          <h2>Parlare è<br />semplice.</h2>
          <ol class="step-list">
            <li><b>1</b><span>Avvia il tuo nodo</span></li>
            <li><b>2</b><span>Scambia un QR o un link</span></li>
            <li><b>3</b><span>Scegli il contatto e scrivi</span></li>
          </ol>
          <button class="primary-button" onclick={running ? openContacts : () => setupOpen = true}>
            {running ? 'Aggiungi un contatto' : 'Inizia'}
          </button>
        </div>
      {:else if messages.length === 0}
        <div class="welcome conversation-empty">
          <div class="welcome-mark small"><MessageCircleMore size={27} /></div>
          <p class="eyebrow">{activeContact.online ? 'Contatto online' : 'Conversazione salvata'}</p>
          <h2>Scrivi a<br />{activeContact.name}.</h2>
          <p class="welcome-copy">{ready ? 'Il canale è cifrato. Il primo messaggio può partire.' : 'Riconnetterò automaticamente il contatto quando sarà disponibile.'}</p>
        </div>
      {:else}
        <div class="day-divider"><span>Conversazione</span></div>
        {#each messages as message (message.id)}
          {#if message.kind === 'nudge'}
            <div class="nudge-message"><Zap size={15} /> {message.mine ? 'Hai inviato un trillo' : 'Trillo!'}</div>
          {:else}
            <div class:mine={message.mine} class="message-row">
              <div class="message-bubble">
                <p>{message.kind === 'file' ? `📎 ${message.body}` : message.body}</p>
                <time>{message.time}</time>
              </div>
            </div>
          {/if}
        {/each}
      {/if}
    </div>

    <footer class="composer-wrap">
      {#if emojiOpen}
        <div class="emoji-picker">
          {#each emoji as item}
            <button aria-label={`Inserisci ${item}`} onclick={() => messageText += item}>{item}</button>
          {/each}
        </div>
      {/if}
      <form class="composer" onsubmit={(event) => { event.preventDefault(); void sendMessage() }}>
        <button type="button" class:active={emojiOpen} class="composer-action" aria-label="Emoticon" disabled={!ready} onclick={() => emojiOpen = !emojiOpen}><Smile size={21} /></button>
        <button type="button" class="composer-action" aria-label="Invia file" disabled={!ready} onclick={chooseFile}><Paperclip size={20} /></button>
        <textarea
          bind:value={messageText}
          rows="1"
          maxlength="4000"
          placeholder={ready ? `Messaggio a ${activeContact?.name}…` : 'In attesa di un canale sicuro…'}
          disabled={!ready}
          onkeydown={(event) => {
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault()
              void sendMessage()
            }
          }}
        ></textarea>
        <button type="submit" class="send-button" aria-label="Invia" disabled={!ready || !messageText.trim()}><Send size={18} /></button>
      </form>
    </footer>
  </section>

  {#if detailsOpen}
    <aside class="details-pane">
      <header><span>Dettagli</span><button aria-label="Chiudi dettagli" onclick={() => detailsOpen = false}><X size={18} /></button></header>
      <div class="profile-focus">
        <span class="avatar profile-avatar">{activeContact?.name.slice(0, 1).toUpperCase() || '?'}</span>
        <strong>{activeContact?.name || 'Nessun contatto'}</strong>
        <small>{activeContact?.peerId ? `${activeContact.peerId.slice(0, 12)}…${activeContact.peerId.slice(-6)}` : 'Aggiungi un contatto per iniziare'}</small>
      </div>
      <div class="security-detail">
        <div class="detail-icon"><ShieldCheck size={20} /></div>
        <span><strong>Cifratura ibrida</strong><small>{ready ? 'X25519 + ML-KEM-768' : 'In attesa del handshake'}</small></span>
        <i class:active={ready}></i>
      </div>
      <div class="security-detail">
        <div class="detail-icon"><Activity size={20} /></div>
        <span><strong>Trasporto</strong><small>{activeContact?.online ? 'QUIC peer-to-peer' : 'Non collegato'}</small></span>
        <i class:active={activeContact?.online}></i>
      </div>
      <section class="local-identity">
        <span class="eyebrow">La tua identità</span>
        <code>{peerId || 'Disponibile dopo l’avvio'}</code>
        <button disabled={!running || linkRequested} onclick={openContacts}>
          <QrCode size={16} /> Mostra QR e link
        </button>
      </section>
      <div class="privacy-note"><LockKeyhole size={14} /><span>Contatti e cronologia restano qui; il contenuto dei messaggi è cifrato.</span></div>
    </aside>
  {/if}
</main>

{#if setupOpen}
  <div class="modal-backdrop">
    <div class="modal" role="dialog" aria-modal="true" aria-labelledby="setup-title">
      {#if running}<button class="modal-close" aria-label="Chiudi" onclick={() => setupOpen = false}><X size={19} /></button>{/if}
      <span class="modal-mark"><MessageCircleMore size={24} /></span>
      <p class="eyebrow">Passaggio 1 di 2</p>
      <h2 id="setup-title">Come vuoi apparire?</h2>
      <p>Non serve registrarsi: creiamo un’identità sul computer e la riutilizziamo ai prossimi avvii.</p>
      <label>Il tuo nome<input bind:value={displayName} maxlength="64" placeholder="Come vuoi apparire" /></label>
      <details>
        <summary>Collegamento diretto avanzato</summary>
        <label>Indirizzo peer <small>opzionale</small><input bind:value={directAddress} placeholder="/ip4/…/udp/…/quic-v1/p2p/…" /></label>
      </details>
      <button class="primary-button wide" disabled={starting || !displayName.trim()} onclick={startNode}>
        {starting ? 'Avvio in corso…' : 'Continua'}
      </button>
      <small class="modal-foot">Sulla stessa rete gli altri nodi vengono trovati automaticamente.</small>
    </div>
  </div>
{/if}

{#if connectOpen}
  <div class="modal-backdrop">
    <div class="modal connect-modal" role="dialog" aria-modal="true" aria-labelledby="connect-title">
      <button class="modal-close" aria-label="Chiudi" onclick={() => connectOpen = false}><X size={19} /></button>
      <span class="modal-mark"><UserRoundPlus size={24} /></span>
      <p class="eyebrow">Passaggio 2 di 2</p>
      <h2 id="connect-title">Collega un amico.</h2>
      <p>Uno mostra il proprio QR o link, l’altro lo scansiona o lo incolla. Basta farlo una volta.</p>

      <section class="share-section">
        <div class="section-title"><span><b>Il tuo contatto</b><small>Invialo alla persona che vuoi aggiungere</small></span><QrCode size={19} /></div>
        {#if ownContactQr}
          <img class="contact-qr" src={ownContactQr} alt="QR del tuo contatto msnnext" />
        {:else}
          <button class="secondary-button" disabled={linkRequested} onclick={createContactLink}>{linkRequested ? 'Creo il QR…' : 'Genera il mio QR'}</button>
        {/if}
        {#if ownContactLink}
          <button class="copy-link" onclick={copyOwnLink}><Copy size={15} /> Copia il link</button>
        {/if}
      </section>

      <div class="or-divider"><span>oppure aggiungi l’altra persona</span></div>

      <label>Link ricevuto<input bind:value={contactLink} placeholder="msnnext://add/…" /></label>
      <button class="scan-button" onclick={scanContactQr}><QrCode size={16} /> Leggi un QR da un’immagine</button>
      <button class="primary-button wide" disabled={!running || !contactLink.trim()} onclick={importContact}>
        <Link2 size={16} /> Verifica e collega
      </button>
    </div>
  </div>
{/if}

{#if toastText}
  <div class="toast" role="status">{toastText}</div>
{/if}
