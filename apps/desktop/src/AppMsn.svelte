<script lang="ts">
  import { invoke, isTauri } from '@tauri-apps/api/core'
  import { listen, type UnlistenFn } from '@tauri-apps/api/event'
  import { open } from '@tauri-apps/plugin-dialog'
  import QRCode from 'qrcode'
  import { onMount, tick } from 'svelte'
  import {
    Activity,
    ChevronDown,
    Copy,
    Info,
    Link2,
    LockKeyhole,
    Menu,
    MessageCircleMore,
    Monitor,
    Moon,
    Paperclip,
    Pencil,
    Plus,
    Power,
    QrCode,
    Radio,
    Send,
    Settings2,
    ShieldCheck,
    Smile,
    Sparkles,
    Sun,
    Trash2,
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
    emoticons: ClientEmoticonSpan[]
  }

  type ClientEmoticonSpan = { start: number; end: number; assetId: string }
  type ClientEmoticon = {
    assetId: string
    name: string
    trigger: string
    mime: string
    dataUrl: string
    animated: boolean
    saved: boolean
  }

  type ChatMessage = {
    id: string
    kind: 'incoming' | 'outgoing' | 'nudge' | 'file'
    body: string
    time: string
    mine: boolean
    emoticons: ClientEmoticonSpan[]
  }

  type ClientEvent =
    | { type: 'started'; peerId: string; displayName: string }
    | { type: 'contactUpdated'; contact: Omit<Contact, 'unread'> }
    | { type: 'conversationLoaded'; peerId: string; messages: ClientMessage[] }
    | { type: 'message'; message: ClientMessage }
    | { type: 'emoticonCatalog'; emoticons: ClientEmoticon[] }
    | { type: 'emoticonOffered'; peerId: string; emoticon: ClientEmoticon }
    | { type: 'emoticonRemoved'; assetId: string }
    | { type: 'contactLink'; link: string }
    | { type: 'attachmentReceived'; peerId: string; path: string }
    | { type: 'attachmentSent'; peerId: string; filename: string }
    | { type: 'contactRemoved'; peerId: string }
    | { type: 'conversationCleared'; peerId: string }
    | { type: 'error'; message: string }
    | { type: 'ready' }
    | { type: 'stopped' }

  type Theme = 'light' | 'dark' | 'system'
  type Emoticon = { glyph: string; shortcut: string; label: string }
  type MessagePart = { text: string; emoticon?: Emoticon; custom?: ClientEmoticon }
  type Profile = { name: string; avatarDataUrl: string | null }

  const emoticons: Emoticon[] = [
    { glyph: '🙂', shortcut: ':)', label: 'Sorriso' },
    { glyph: '😄', shortcut: ':D', label: 'Risata' },
    { glyph: '😉', shortcut: ';)', label: 'Occhiolino' },
    { glyph: '😛', shortcut: ':P', label: 'Linguaccia' },
    { glyph: '😢', shortcut: ':(', label: 'Triste' },
    { glyph: '😮', shortcut: ':o', label: 'Sorpresa' },
    { glyph: '❤️', shortcut: '<3', label: 'Cuore' },
    { glyph: '😎', shortcut: '8)', label: 'Forte' },
  ]

  const savedTheme = typeof localStorage === 'undefined' ? null : localStorage.getItem('msnnext-theme')
  let theme: Theme = savedTheme === 'light' || savedTheme === 'dark' || savedTheme === 'system'
    ? savedTheme
    : 'system'
  let displayName = typeof localStorage === 'undefined'
    ? 'Amico'
    : localStorage.getItem('msnnext-name') || 'Amico'
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
  let customEmoticons: ClientEmoticon[] = []
  let offeredEmoticons: ClientEmoticon[] = []
  let running = false
  let starting = false
  let setupOpen = true
  let profileOpen = false
  let avatarDataUrl = ''
  let connectOpen = false
  let detailsOpen = false
  let emojiOpen = false
  let emoticonCreateOpen = false
  let emoticonSaveOpen = false
  let emoticonPath = ''
  let emoticonTrigger = ''
  let emoticonToSave: ClientEmoticon | undefined
  let pendingEmoticonAction = ''
  let fileSending = false
  let contactName = ''
  let contactNamePeer = ''
  let rosterOpen = false
  let onlineOpen = true
  let offlineOpen = true
  let linkRequested = false
  let toastText = ''
  let toastTimer: ReturnType<typeof setTimeout>
  let messageList: HTMLDivElement

  $: activeContact = contacts.find((contact) => contact.peerId === selectedPeerId)
  $: messages = selectedPeerId ? conversations[selectedPeerId] || [] : []
  $: visibleContacts = contacts.filter((contact) =>
    contact.name.toLocaleLowerCase().includes(searchQuery.trim().toLocaleLowerCase())
  )
  $: onlineContacts = visibleContacts.filter((contact) => contact.online)
  $: offlineContacts = visibleContacts.filter((contact) => !contact.online)
  $: ready = Boolean(activeContact?.online && activeContact?.secure)

  onMount(() => {
    const media = window.matchMedia('(prefers-color-scheme: dark)')
    const syncSystemTheme = () => {
      if (theme === 'system') applyTheme()
    }
    media.addEventListener('change', syncSystemTheme)
    applyTheme()

    let unlisten: UnlistenFn | undefined
    if (isTauri()) void initializeApp().then((stop) => unlisten = stop)

    return () => {
      unlisten?.()
      media.removeEventListener('change', syncSystemTheme)
    }
  })

  async function initializeApp() {
    const stop = await listen<ClientEvent>('client-event', ({ payload }) => handleEvent(payload))
    try {
      let profile = await invoke<Profile | null>('profile_load')
      if (!profile) {
        const savedName = localStorage.getItem('msnnext-name')
        if (savedName) profile = await invoke<Profile>('profile_save', {
          name: savedName,
          avatarPath: null,
          clearAvatar: false,
        })
      }
      if (!profile) {
        setupOpen = true
        return stop
      }
      displayName = profile.name
      avatarDataUrl = profile.avatarDataUrl || ''
      const isRunning = await invoke<boolean>('node_status')
      running = isRunning
      setupOpen = false
      if (!isRunning) await startNode(false)
    } catch (error) {
      setupOpen = true
      showToast(String(error))
    }
    return stop
  }

  function applyTheme() {
    const resolved = theme === 'system'
      ? (window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light')
      : theme
    document.documentElement.dataset.theme = resolved
    document.documentElement.style.colorScheme = resolved
  }

  function setTheme(next: Theme) {
    theme = next
    localStorage.setItem('msnnext-theme', next)
    applyTheme()
  }

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
    if (event.type === 'emoticonCatalog') {
      for (const emoticon of event.emoticons) upsertCustomEmoticon(emoticon)
      if (pendingEmoticonAction) {
        showToast(pendingEmoticonAction)
        pendingEmoticonAction = ''
        emoticonCreateOpen = false
        emoticonSaveOpen = false
        emoticonToSave = undefined
      }
      return
    }
    if (event.type === 'emoticonOffered') {
      offeredEmoticons = [
        ...offeredEmoticons.filter((item) => item.assetId !== event.emoticon.assetId),
        event.emoticon,
      ]
      showToast(`Nuova emoticon da ${contacts.find((item) => item.peerId === event.peerId)?.name || 'un contatto'}`)
      return
    }
    if (event.type === 'contactLink') {
      ownContactLink = event.link
      linkRequested = false
      void QRCode.toDataURL(event.link, {
        width: 220,
        margin: 1,
        color: { dark: '#10284a', light: '#ffffff' },
      }).then((qr) => ownContactQr = qr)
      return
    }
    if (event.type === 'attachmentReceived') {
      showToast(`File ricevuto: ${event.path}`)
      return
    }
    if (event.type === 'attachmentSent') {
      fileSending = false
      showToast(`File inviato: ${event.filename}`)
      return
    }
    if (event.type === 'emoticonRemoved') {
      customEmoticons = customEmoticons.filter((item) => item.assetId !== event.assetId)
      pendingEmoticonAction = ''
      emoticonSaveOpen = false
      emoticonToSave = undefined
      showToast('Emoticon eliminata')
      return
    }
    if (event.type === 'conversationCleared') {
      conversations = { ...conversations, [event.peerId]: [] }
      showToast('Cronologia eliminata')
      return
    }
    if (event.type === 'contactRemoved') {
      contacts = contacts.filter((contact) => contact.peerId !== event.peerId)
      const { [event.peerId]: _removed, ...remaining } = conversations
      conversations = remaining
      if (selectedPeerId === event.peerId) selectedPeerId = contacts[0]?.peerId || ''
      detailsOpen = false
      showToast('Contatto eliminato')
      return
    }
    if (event.type === 'error') {
      linkRequested = false
      fileSending = false
      pendingEmoticonAction = ''
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
      emoticons: message.emoticons || [],
      mine: message.direction === 'out',
      time: new Intl.DateTimeFormat('it', {
        hour: '2-digit',
        minute: '2-digit',
      }).format(new Date(message.timestampMs)),
    }
  }

  function upsertCustomEmoticon(emoticon: ClientEmoticon) {
    customEmoticons = [
      ...customEmoticons.filter((item) => item.assetId !== emoticon.assetId),
      emoticon,
    ].sort((a, b) => a.trigger.localeCompare(b.trigger))
    offeredEmoticons = offeredEmoticons.filter((item) => item.assetId !== emoticon.assetId)
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

  function selectContact(id: string) {
    selectedPeerId = id
    contactName = contacts.find((contact) => contact.peerId === id)?.name || ''
    contactNamePeer = id
    rosterOpen = false
    contacts = contacts.map((contact) =>
      contact.peerId === id ? { ...contact, unread: 0 } : contact
    )
    scrollMessages()
  }

  function openConversationDetails() {
    if (activeContact && contactNamePeer !== activeContact.peerId) {
      contactName = activeContact.name
      contactNamePeer = activeContact.peerId
    }
    detailsOpen = !detailsOpen
  }

  function contactSubtitle(contact: Contact) {
    const last = conversations[contact.peerId]?.at(-1)
    if (last) {
      if (last.kind === 'nudge') return '⚡ Trillo'
      if (last.kind === 'file') return `📎 ${last.body}`
      return last.body
    }
    return contact.secure ? 'Conversazione protetta' : contact.online ? 'Collegamento…' : 'Non in linea'
  }

  function builtinMessageParts(text: string): MessagePart[] {
    const escaped = emoticons
      .map((item) => item.shortcut.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'))
      .join('|')
    const matcher = new RegExp(`(${escaped})`, 'g')
    return text.split(matcher).filter(Boolean).map((part) => ({
      text: part,
      emoticon: emoticons.find((item) => item.shortcut === part),
    }))
  }

  function textIndexAtByteOffset(text: string, offset: number) {
    let bytes = 0
    let index = 0
    for (const character of text) {
      if (bytes >= offset) break
      bytes += new TextEncoder().encode(character).length
      index += character.length
    }
    return index
  }

  function messageParts(message: ChatMessage): MessagePart[] {
    if (!message.emoticons.length) return builtinMessageParts(message.body)
    const parts: MessagePart[] = []
    let cursor = 0
    for (const span of [...message.emoticons].sort((a, b) => a.start - b.start)) {
      const start = textIndexAtByteOffset(message.body, span.start)
      const end = textIndexAtByteOffset(message.body, span.end)
      if (start < cursor || end <= start) continue
      parts.push(...builtinMessageParts(message.body.slice(cursor, start)))
      const custom = [...customEmoticons, ...offeredEmoticons]
        .find((item) => item.assetId === span.assetId)
      parts.push(custom
        ? { text: message.body.slice(start, end), custom }
        : { text: message.body.slice(start, end) })
      cursor = end
    }
    parts.push(...builtinMessageParts(message.body.slice(cursor)))
    return parts
  }

  function insertEmoticon(item: Emoticon) {
    const spacer = messageText && !messageText.endsWith(' ') ? ' ' : ''
    messageText += `${spacer}${item.shortcut} `
  }

  function insertCustomEmoticon(item: ClientEmoticon) {
    const spacer = messageText && !messageText.endsWith(' ') ? ' ' : ''
    messageText += `${spacer}${item.trigger} `
  }

  function scrollMessages() {
    void tick().then(() =>
      messageList?.scrollTo({ top: messageList.scrollHeight, behavior: 'smooth' })
    )
  }

  async function startNode(saveProfile = true) {
    if (!displayName.trim()) return
    starting = true
    try {
      if (saveProfile) {
        const profile = await invoke<Profile>('profile_save', {
          name: displayName.trim(), avatarPath: null, clearAvatar: false,
        })
        avatarDataUrl = profile.avatarDataUrl || ''
      }
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
    try {
      await invoke('node_stop')
      running = false
      contacts = contacts.map((contact) => ({ ...contact, online: false, secure: false }))
    } catch (error) {
      showToast(String(error))
    }
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
      fileSending = true
      await invoke('node_send_file', { peerId: selectedPeerId, path: selected })
    } catch (error) {
      showToast(String(error))
    }
  }

  async function chooseEmoticonFile() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: 'Emoticon', extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp'] }],
    })
    if (!selected || Array.isArray(selected)) return
    emoticonPath = selected
    emoticonTrigger = ''
    emoticonCreateOpen = true
  }

  async function createCustomEmoticon() {
    if (!emoticonPath || !emoticonTrigger.trim()) return
    try {
      pendingEmoticonAction = 'Emoticon creata'
      await invoke('node_create_emoticon', {
        path: emoticonPath,
        trigger: emoticonTrigger.trim(),
      })
    } catch (error) {
      pendingEmoticonAction = ''
      showToast(String(error))
    }
  }

  function openSaveEmoticon(emoticon: ClientEmoticon) {
    emoticonToSave = emoticon
    emoticonTrigger = emoticon.trigger
    emoticonSaveOpen = true
  }

  async function saveReceivedEmoticon() {
    if (!emoticonToSave || !emoticonTrigger.trim()) return
    try {
      pendingEmoticonAction = emoticonToSave.saved ? 'Scorciatoia aggiornata' : 'Emoticon salvata'
      await invoke(emoticonToSave.saved ? 'node_update_emoticon' : 'node_save_emoticon', {
        assetId: emoticonToSave.assetId,
        trigger: emoticonTrigger.trim(),
      })
    } catch (error) {
      pendingEmoticonAction = ''
      showToast(String(error))
    }
  }

  async function deleteEmoticon() {
    if (!emoticonToSave?.saved || !confirm('Eliminare questa emoticon?')) return
    pendingEmoticonAction = 'Eliminazione emoticon…'
    try {
      await invoke('node_delete_emoticon', { assetId: emoticonToSave.assetId })
    } catch (error) {
      pendingEmoticonAction = ''
      showToast(String(error))
    }
  }

  async function saveProfile(avatarPath: string | null = null, clearAvatar = false) {
    if (!displayName.trim()) return
    try {
      const profile = await invoke<Profile>('profile_save', {
        name: displayName.trim(), avatarPath, clearAvatar,
      })
      displayName = profile.name
      avatarDataUrl = profile.avatarDataUrl || ''
      localStorage.setItem('msnnext-name', profile.name)
      profileOpen = false
      showToast('Profilo aggiornato')
    } catch (error) {
      showToast(String(error))
    }
  }

  async function chooseAvatar() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: 'Immagine profilo', extensions: ['png', 'jpg', 'jpeg', 'webp'] }],
    })
    if (selected && !Array.isArray(selected)) await saveProfile(selected, false)
  }

  async function renameContact() {
    if (!selectedPeerId || !contactName.trim()) return
    try {
      await invoke('node_rename_contact', { peerId: selectedPeerId, name: contactName.trim() })
    } catch (error) { showToast(String(error)) }
  }

  async function clearConversation() {
    if (!selectedPeerId || !confirm('Eliminare tutta la cronologia di questa chat?')) return
    try { await invoke('node_clear_conversation', { peerId: selectedPeerId }) }
    catch (error) { showToast(String(error)) }
  }

  async function deleteContact() {
    if (!selectedPeerId || !confirm(`Eliminare ${activeContact?.name || 'questo contatto'} e la sua chat?`)) return
    try { await invoke('node_delete_contact', { peerId: selectedPeerId }) }
    catch (error) { showToast(String(error)) }
  }

  async function importContact() {
    if (!contactLink.trim().startsWith('msnnext://add/')) {
      showToast('Il link contatto non è valido')
      return
    }
    try {
      await invoke('node_import_contact', { link: contactLink.trim() })
      contactLink = ''
      showToast('Contatto aggiunto. Provo a collegarlo…')
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

<main class:details-open={detailsOpen} class:roster-open={rosterOpen} class="app-frame">
  <header class="app-titlebar">
    <div class="wordmark" aria-label="msnnext">
      <span class="wordmark-people" aria-hidden="true"><i></i><i></i></span>
      <strong>msnnext</strong>
      <small>messenger</small>
    </div>
    <div class="titlebar-tools">
      <div class="theme-switcher" role="group" aria-label="Tema">
        <button class:active={theme === 'light'} aria-label="Tema chiaro" title="Tema chiaro" onclick={() => setTheme('light')}>
          <Sun size={14} /><span>Chiaro</span>
        </button>
        <button class:active={theme === 'dark'} aria-label="Tema scuro" title="Tema scuro" onclick={() => setTheme('dark')}>
          <Moon size={14} /><span>Scuro</span>
        </button>
        <button class:active={theme === 'system'} aria-label="Tema di sistema" title="Usa il tema del sistema" onclick={() => setTheme('system')}>
          <Monitor size={14} /><span>Sistema</span>
        </button>
      </div>
      <span class:online={running} class="node-state"><i></i>{running ? 'Connesso' : 'Non connesso'}</span>
      <button class:online={running} class="power-button" aria-label={running ? 'Disconnetti' : 'Connetti'} title={running ? 'Disconnetti' : 'Connetti'} onclick={running ? stopNode : () => setupOpen = true}>
        <Power size={16} />
      </button>
    </div>
  </header>

  <div class="workspace">
    <aside class="contacts-pane">
      <header class="my-profile">
        <div class="avatar-shell me">
          {#if avatarDataUrl}<img src={avatarDataUrl} alt="" />{:else}<span>{displayName.slice(0, 1).toUpperCase()}</span>{/if}
          <i class:online={running}></i>
        </div>
        <div class="profile-copy">
          <strong>{displayName}</strong>
          <span>{running ? 'Disponibile' : 'Non in linea'}</span>
          <small>{running ? 'Pronto per parlare' : 'Avvia messenger per collegarti'}</small>
        </div>
        <button aria-label="Modifica profilo" title="Modifica profilo" onclick={() => profileOpen = true}>
          <Settings2 size={17} />
        </button>
      </header>

      <div class="roster-actions">
        <label class="search-field">
          <span>Cerca tra i contatti</span>
          <input bind:value={searchQuery} aria-label="Cerca contatti" placeholder="Cerca un contatto…" />
        </label>
        <button class="add-contact" aria-label="Aggiungi un contatto" title="Aggiungi un contatto" onclick={openContacts}>
          <UserRoundPlus size={18} />
        </button>
      </div>

      <section class="contact-list" aria-label="Lista contatti">
        {#if visibleContacts.length}
          <section class="contact-group">
            <button class="group-heading" aria-expanded={onlineOpen} onclick={() => onlineOpen = !onlineOpen}>
              <ChevronDown class={onlineOpen ? '' : 'closed'} size={15} />
              <strong>Online</strong>
              <span>{onlineContacts.length}</span>
            </button>
            {#if onlineOpen}
              {#each onlineContacts as contact (contact.peerId)}
                <button class:active={contact.peerId === selectedPeerId} class="contact-row" onclick={() => selectContact(contact.peerId)}>
                  <span class="avatar-shell contact-avatar">
                    <span>{contact.name.slice(0, 1).toUpperCase()}</span>
                    <i class:online={contact.online}></i>
                  </span>
                  <span class="contact-copy">
                    <strong>{contact.name}</strong>
                    <small>{contactSubtitle(contact)}</small>
                  </span>
                  {#if contact.unread}<b class="unread">{contact.unread}</b>{/if}
                </button>
              {/each}
            {/if}
          </section>

          <section class="contact-group offline-group">
            <button class="group-heading" aria-expanded={offlineOpen} onclick={() => offlineOpen = !offlineOpen}>
              <ChevronDown class={offlineOpen ? '' : 'closed'} size={15} />
              <strong>Offline</strong>
              <span>{offlineContacts.length}</span>
            </button>
            {#if offlineOpen}
              {#each offlineContacts as contact (contact.peerId)}
                <button class:active={contact.peerId === selectedPeerId} class="contact-row" onclick={() => selectContact(contact.peerId)}>
                  <span class="avatar-shell contact-avatar offline">
                    <span>{contact.name.slice(0, 1).toUpperCase()}</span>
                    <i></i>
                  </span>
                  <span class="contact-copy">
                    <strong>{contact.name}</strong>
                    <small>{contactSubtitle(contact)}</small>
                  </span>
                  {#if contact.unread}<b class="unread">{contact.unread}</b>{/if}
                </button>
              {/each}
            {/if}
          </section>
        {:else if contacts.length}
          <div class="empty-contacts compact">
            <strong>Nessun risultato</strong>
            <p>Prova a cercare con un altro nome.</p>
          </div>
        {:else}
          <div class="empty-contacts">
            <span class="empty-people" aria-hidden="true"><i></i><i></i></span>
            <strong>La tua lista è vuota</strong>
            <p>Aggiungi un amico con il suo QR o con un link.</p>
            <button onclick={openContacts}><Plus size={15} /> Aggiungi contatto</button>
          </div>
        {/if}
      </section>

      <footer class="roster-footer">
        <button onclick={openContacts}><UserRoundPlus size={15} /> Aggiungi</button>
        <span>{onlineContacts.length} online</span>
      </footer>
    </aside>

    <button class="roster-scrim" aria-label="Chiudi lista contatti" onclick={() => rosterOpen = false}></button>

    <section class="conversation">
      <header class="conversation-header">
        <div class="conversation-person">
          <button class="mobile-roster-button" aria-label="Apri lista contatti" onclick={() => rosterOpen = true}>
            <Menu size={19} />
          </button>
          <div class="avatar-shell large">
            <span>{activeContact?.name.slice(0, 1).toUpperCase() || '?'}</span>
            <i class:online={activeContact?.online}></i>
          </div>
          <span>
            <strong>{activeContact?.name || 'msnnext'}</strong>
            <small>
              {ready ? 'Disponibile · conversazione protetta' : activeContact?.online ? 'Sto preparando la conversazione…' : activeContact ? 'Non in linea' : 'Scegli una persona dalla lista'}
            </small>
          </span>
        </div>
        <div class="header-actions">
          <span class:secure={ready} class="security-badge"><ShieldCheck size={14} />{ready ? 'Protetta' : 'In attesa'}</span>
          <button class:active={detailsOpen} class="header-tool" aria-label="Dettagli conversazione" title="Dettagli conversazione" onclick={openConversationDetails}>
            <Info size={17} />
          </button>
        </div>
      </header>

      <div class="conversation-stage">
        <div class="messages" bind:this={messageList} aria-live="polite">
          {#if !activeContact}
            <div class="welcome">
              <div class="welcome-illustration" aria-hidden="true">
                <span class="person one"></span>
                <span class="person two"></span>
                <i class="orbit one"></i>
                <i class="orbit two"></i>
              </div>
              <p class="welcome-kicker">Bentornato su msnnext</p>
              <h1>Le persone che vuoi.<br />Nient’altro.</h1>
              <p>Scegli un contatto dalla lista, oppure aggiungi un amico per iniziare.</p>
              <button class="primary-button" onclick={running ? openContacts : () => setupOpen = true}>
                {running ? 'Aggiungi un contatto' : 'Vai online'}
              </button>
            </div>
          {:else if messages.length === 0}
            <div class="conversation-empty">
              <div class="avatar-shell hero-avatar">
                <span>{activeContact.name.slice(0, 1).toUpperCase()}</span>
                <i class:online={activeContact.online}></i>
              </div>
              <h2>{activeContact.name}</h2>
              <p>{ready ? 'È online. Scrivi il primo messaggio o manda un trillo.' : 'Quando tornerà online potrete riprendere a parlare.'}</p>
            </div>
          {:else}
            <div class="session-start"><span>Inizio della conversazione</span></div>
            {#each messages as message (message.id)}
              {#if message.kind === 'nudge'}
                <div class="nudge-message">
                  <span><Zap size={18} /></span>
                  <p><strong>{message.mine ? 'Hai inviato un trillo!' : `${activeContact.name} ti ha inviato un trillo!`}</strong><small>La finestra ha fatto un piccolo salto.</small></p>
                  <time>{message.time}</time>
                </div>
              {:else}
                <article class:mine={message.mine} class:file-message={message.kind === 'file'} class="message-line">
                  <header>
                    <strong>{message.mine ? displayName : activeContact.name}</strong>
                    <time>{message.time}</time>
                  </header>
                  {#if message.kind === 'file'}
                    <div class="file-line"><Paperclip size={17} /><span><b>File condiviso</b><small>{message.body}</small></span></div>
                  {:else}
                    <p>
                      {#each messageParts(message) as part}
                        {#if part.custom}
                          <img class="custom-inline-emoticon" src={part.custom.dataUrl} alt={part.custom.name} title={`${part.custom.name} (${part.custom.trigger})`} />
                        {:else if part.emoticon}
                          <span class="inline-emoticon" title={`${part.emoticon.label} (${part.emoticon.shortcut})`}>{part.emoticon.glyph}</span>
                        {:else}{part.text}{/if}
                      {/each}
                    </p>
                  {/if}
                </article>
              {/if}
            {/each}
          {/if}
        </div>

        {#if detailsOpen}
          <aside class="details-pane">
            <header><strong>Dettagli</strong><button aria-label="Chiudi dettagli" onclick={() => detailsOpen = false}><X size={17} /></button></header>
            <div class="detail-profile">
              <div class="avatar-shell profile-avatar">
                <span>{activeContact?.name.slice(0, 1).toUpperCase() || displayName.slice(0, 1).toUpperCase()}</span>
                <i class:online={activeContact?.online || running}></i>
              </div>
              <strong>{activeContact?.name || displayName}</strong>
              <small>{activeContact ? (activeContact.online ? 'Disponibile' : 'Non in linea') : (running ? 'Online' : 'Non in linea')}</small>
            </div>
            <section class="detail-section">
              <h3>Sicurezza</h3>
              <div class="detail-row">
                <span><ShieldCheck size={18} /></span>
                <p><strong>Conversazione protetta</strong><small>{ready ? 'Chiavi verificate per questa sessione' : 'Disponibile quando il contatto è online'}</small></p>
                <i class:active={ready}></i>
              </div>
              <div class="detail-row">
                <span><Activity size={18} /></span>
                <p><strong>Collegamento diretto</strong><small>{activeContact?.online ? 'Attivo tra i vostri dispositivi' : 'Non collegato'}</small></p>
                <i class:active={activeContact?.online}></i>
              </div>
              <details class="technical-details">
                <summary>Dettagli tecnici</summary>
                <p>Cifratura ibrida X25519 + ML-KEM-768. Trasporto QUIC peer-to-peer.</p>
              </details>
            </section>
            <section class="detail-section identity-detail">
              <h3>La tua identità</h3>
              <code>{peerId || 'Disponibile dopo l’avvio'}</code>
              <button disabled={!running || linkRequested} onclick={openContacts}><QrCode size={15} /> Mostra QR e link</button>
            </section>
            {#if activeContact}
              <section class="detail-section contact-management">
                <h3>Gestione contatto</h3>
                <label>Nome personale<input bind:value={contactName} maxlength="64" placeholder={activeContact.name} /></label>
                <button onclick={renameContact}><Pencil size={14} /> Salva nome</button>
                <button onclick={clearConversation}><Trash2 size={14} /> Elimina solo la chat</button>
                <button class="danger-button" onclick={deleteContact}><Trash2 size={14} /> Elimina contatto e chat</button>
              </section>
            {/if}
            <div class="privacy-note"><LockKeyhole size={14} /><span>Contatti e cronologia restano su questo dispositivo.</span></div>
          </aside>
        {/if}
      </div>

      <footer class="composer-wrap">
        {#if emojiOpen}
          <div class="emoji-picker">
            <header>
              <span><strong>Emoticon</strong><small>Scegli oppure digita la scorciatoia</small></span>
              <span class="emoji-header-actions">
                <button class="create-emoticon-button" onclick={chooseEmoticonFile}><Plus size={14} /> Crea</button>
                <button aria-label="Chiudi emoticon" onclick={() => emojiOpen = false}><X size={15} /></button>
              </span>
            </header>
            {#if customEmoticons.length}
              <small class="emoji-section-label">Le tue emoticon</small>
              <div class="emoji-grid custom-emoji-grid">
                {#each customEmoticons as item (item.assetId)}
                  <div class="custom-emoji-item">
                    <button aria-label={`Inserisci ${item.name}`} title={`${item.name} · ${item.trigger}`} onclick={() => insertCustomEmoticon(item)}>
                      <img src={item.dataUrl} alt="" /><small>{item.trigger}</small>
                    </button>
                    <button class="edit-emoticon" aria-label={`Modifica ${item.name}`} title="Modifica o elimina" onclick={() => openSaveEmoticon(item)}><Pencil size={11} /></button>
                  </div>
                {/each}
              </div>
            {/if}
            {#if offeredEmoticons.length}
              <small class="emoji-section-label">Ricevute da salvare</small>
              <div class="received-emoji-list">
                {#each offeredEmoticons as item (item.assetId)}
                  <div><img src={item.dataUrl} alt={item.name} /><span><strong>{item.name}</strong><small>{item.trigger}</small></span><button onclick={() => openSaveEmoticon(item)}>Salva</button></div>
                {/each}
              </div>
            {/if}
            <small class="emoji-section-label">Classiche</small>
            <div class="emoji-grid">
              {#each emoticons as item}
                <button aria-label={`Inserisci ${item.label}`} title={`${item.label} · ${item.shortcut}`} onclick={() => insertEmoticon(item)}>
                  <span>{item.glyph}</span><small>{item.shortcut}</small>
                </button>
              {/each}
            </div>
            <p>Le scorciatoie diventano emoticon nella conversazione.</p>
          </div>
        {/if}

        <div class="chat-toolbar" aria-label="Strumenti conversazione">
          <button class:active={emojiOpen} disabled={!ready} onclick={() => emojiOpen = !emojiOpen}><Smile size={18} /><span>Emoticon</span></button>
          <button class="nudge-tool" disabled={!ready} onclick={sendNudge}><Zap size={18} /><span>Trillo</span></button>
          <button disabled={!ready || fileSending} onclick={chooseFile}><Paperclip size={18} /><span>{fileSending ? 'Invio…' : 'Invia file'}</span></button>
        </div>
        <form class="composer" onsubmit={(event) => { event.preventDefault(); void sendMessage() }}>
          <textarea
            bind:value={messageText}
            rows="2"
            maxlength="4000"
            placeholder={ready ? `Scrivi a ${activeContact?.name}…` : activeContact ? 'Il contatto non è disponibile.' : 'Scegli un contatto per scrivere.'}
            disabled={!ready}
            onkeydown={(event) => {
              if (event.key === 'Enter' && !event.shiftKey) {
                event.preventDefault()
                void sendMessage()
              }
            }}
          ></textarea>
          <button type="submit" class="send-button" disabled={!ready || !messageText.trim()}><Send size={17} /> Invia</button>
        </form>
        <small class="composer-hint">Invio per spedire · Maiusc+Invio per andare a capo</small>
      </footer>
    </section>
  </div>
</main>

{#if setupOpen}
  <div class="modal-backdrop">
    <div class="modal-theme-switcher" role="group" aria-label="Tema della finestra">
      <button class:active={theme === 'light'} onclick={() => setTheme('light')}><Sun size={14} /> Chiaro</button>
      <button class:active={theme === 'dark'} onclick={() => setTheme('dark')}><Moon size={14} /> Scuro</button>
      <button class:active={theme === 'system'} onclick={() => setTheme('system')}><Monitor size={14} /> Sistema</button>
    </div>
    <div class="modal setup-modal" role="dialog" aria-modal="true" aria-labelledby="setup-title">
      {#if running}<button class="modal-close" aria-label="Chiudi" onclick={() => setupOpen = false}><X size={18} /></button>{/if}
      <div class="modal-sky">
        <span class="modal-people" aria-hidden="true"><i></i><i></i></span>
        <div><strong>msnnext</strong><small>messenger</small></div>
      </div>
      <div class="modal-body">
        <p class="step-label">Prima di andare online</p>
        <h2 id="setup-title">Come vuoi apparire?</h2>
        <p>Scegli il nome che vedranno i tuoi amici. Non serve registrarsi.</p>
        <label>Il tuo nome<input bind:value={displayName} maxlength="64" placeholder="Scrivi il tuo nome" /></label>
        <details>
          <summary>Collegamento diretto avanzato</summary>
          <label>Indirizzo peer <small>facoltativo</small><input bind:value={directAddress} placeholder="/ip4/…/udp/…/quic-v1/p2p/…" /></label>
        </details>
        <button class="primary-button wide" disabled={starting || !displayName.trim()} onclick={() => startNode()}>
          {starting ? 'Connessione in corso…' : 'Vai online'}
        </button>
        <small class="modal-foot">Sulla stessa rete gli amici vengono trovati automaticamente.</small>
      </div>
    </div>
  </div>
{/if}

{#if connectOpen}
  <div class="modal-backdrop">
    <div class="modal-theme-switcher" role="group" aria-label="Tema della finestra">
      <button class:active={theme === 'light'} onclick={() => setTheme('light')}><Sun size={14} /> Chiaro</button>
      <button class:active={theme === 'dark'} onclick={() => setTheme('dark')}><Moon size={14} /> Scuro</button>
      <button class:active={theme === 'system'} onclick={() => setTheme('system')}><Monitor size={14} /> Sistema</button>
    </div>
    <div class="modal connect-modal" role="dialog" aria-modal="true" aria-labelledby="connect-title">
      <button class="modal-close" aria-label="Chiudi" onclick={() => connectOpen = false}><X size={18} /></button>
      <div class="modal-heading">
        <span><UserRoundPlus size={23} /></span>
        <div><p class="step-label">Aggiungi un contatto</p><h2 id="connect-title">Trova un amico</h2></div>
      </div>
      <p>Condividi il tuo QR, oppure usa quello che hai ricevuto.</p>

      <section class="share-section">
        <header><span><strong>Il tuo contatto</strong><small>Fallo inquadrare o invialo come immagine</small></span><QrCode size={19} /></header>
        {#if ownContactQr}
          <img class="contact-qr" src={ownContactQr} alt="QR del tuo contatto msnnext" />
        {:else}
          <button class="secondary-button" disabled={linkRequested} onclick={createContactLink}>{linkRequested ? 'Preparo il QR…' : 'Crea il mio QR'}</button>
        {/if}
        {#if ownContactLink}
          <button class="copy-link" onclick={copyOwnLink}><Copy size={15} /> Copia il link</button>
        {/if}
      </section>

      <div class="or-divider"><span>oppure aggiungi l’altra persona</span></div>
      <label>Link ricevuto<input bind:value={contactLink} placeholder="msnnext://add/…" /></label>
      <button class="scan-button" onclick={scanContactQr}><QrCode size={16} /> Leggi un QR da un’immagine</button>
      <button class="primary-button wide" disabled={!running || !contactLink.trim()} onclick={importContact}>
        <Link2 size={16} /> Aggiungi alla lista
      </button>
    </div>
  </div>
{/if}

{#if emoticonCreateOpen}
  <div class="modal-backdrop">
    <div class="modal emoticon-modal" role="dialog" aria-modal="true" aria-labelledby="create-emoticon-title">
      <button class="modal-close" aria-label="Chiudi" onclick={() => emoticonCreateOpen = false}><X size={18} /></button>
      <div class="modal-heading">
        <span><Smile size={23} /></span>
        <div><p class="step-label">Emoticon personale</p><h2 id="create-emoticon-title">Crea la tua emoticon</h2></div>
      </div>
      <p>Scegli la scorciatoia che la farà apparire nei messaggi, per esempio <b>:ciao:</b>.</p>
      <label>Scorciatoia<input bind:value={emoticonTrigger} maxlength="32" placeholder=":mia:" /></label>
      <button class="primary-button wide" disabled={!emoticonTrigger.trim()} onclick={createCustomEmoticon}>Crea emoticon</button>
    </div>
  </div>
{/if}

{#if emoticonSaveOpen && emoticonToSave}
  <div class="modal-backdrop">
    <div class="modal emoticon-modal" role="dialog" aria-modal="true" aria-labelledby="save-emoticon-title">
      <button class="modal-close" aria-label="Chiudi" onclick={() => emoticonSaveOpen = false}><X size={18} /></button>
      <div class="received-emoticon-preview"><img src={emoticonToSave.dataUrl} alt={emoticonToSave.name} /></div>
      <p class="step-label">{emoticonToSave.saved ? 'La tua emoticon' : 'Emoticon ricevuta'}</p>
      <h2 id="save-emoticon-title">{emoticonToSave.saved ? `Modifica ${emoticonToSave.name}` : `Salva ${emoticonToSave.name}`}</h2>
      <p>{emoticonToSave.saved ? 'Cambia la scorciatoia che la fa apparire nei messaggi.' : 'Puoi mantenere la scorciatoia suggerita o sceglierne una tua.'}</p>
      <label>Scorciatoia<input bind:value={emoticonTrigger} maxlength="32" placeholder=":emoticon:" /></label>
      <button class="primary-button wide" disabled={!emoticonTrigger.trim() || !!pendingEmoticonAction} onclick={saveReceivedEmoticon}>{emoticonToSave.saved ? 'Salva modifica' : 'Salva nelle mie emoticon'}</button>
      {#if emoticonToSave.saved}<button class="danger-button wide" disabled={!!pendingEmoticonAction} onclick={deleteEmoticon}><Trash2 size={15} /> Elimina emoticon</button>{/if}
    </div>
  </div>
{/if}

{#if profileOpen}
  <div class="modal-backdrop">
    <div class="modal profile-modal" role="dialog" aria-modal="true" aria-labelledby="profile-title">
      <button class="modal-close" aria-label="Chiudi" onclick={() => profileOpen = false}><X size={18} /></button>
      <div class="profile-editor-avatar avatar-shell">
        {#if avatarDataUrl}<img src={avatarDataUrl} alt="Avatar personale" />{:else}<span>{displayName.slice(0, 1).toUpperCase()}</span>{/if}
      </div>
      <p class="step-label">Il tuo profilo</p>
      <h2 id="profile-title">Come appari agli amici</h2>
      <label>Nome<input bind:value={displayName} maxlength="64" /></label>
      <div class="profile-avatar-actions">
        <button class="secondary-button" onclick={chooseAvatar}>Scegli avatar</button>
        {#if avatarDataUrl}<button class="secondary-button" onclick={() => saveProfile(null, true)}>Rimuovi</button>{/if}
      </div>
      <button class="primary-button wide" disabled={!displayName.trim()} onclick={() => saveProfile()}>Salva profilo</button>
    </div>
  </div>
{/if}

{#if toastText}
  <div class="toast" role="status"><MessageCircleMore size={16} />{toastText}</div>
{/if}
