<script lang="ts">
  import { invoke, isTauri } from '@tauri-apps/api/core'
  import { getVersion } from '@tauri-apps/api/app'
  import { listen, type UnlistenFn } from '@tauri-apps/api/event'
  import { getCurrentWebview } from '@tauri-apps/api/webview'
  import { getCurrentWindow, UserAttentionType } from '@tauri-apps/api/window'
  import { Image } from '@tauri-apps/api/image'
  import { PhysicalPosition } from '@tauri-apps/api/dpi'
  import { open, save } from '@tauri-apps/plugin-dialog'
  import { relaunch } from '@tauri-apps/plugin-process'
  import { check, type Update } from '@tauri-apps/plugin-updater'
  import QRCode from 'qrcode'
  import { onMount, tick } from 'svelte'
  import {
    Activity,
    BellOff,
    CheckCircle2,
    Copy,
    Database,
    Download,
    ExternalLink,
    Info,
    Link2,
    LockKeyhole,
    Menu,
    MessageCircleMore,
    Minus,
    Monitor,
    Moon,
    Palette,
    Paperclip,
    Pencil,
    Plus,
    Power,
    QrCode,
    Radio,
    RefreshCw,
    Send,
    Settings2,
    ShieldCheck,
    Smile,
    Sparkles,
    Square,
    Sun,
    Trash2,
    Upload,
    UserRound,
    UserRoundPlus,
    UsersRound,
    X,
    Zap,
  } from '@lucide/svelte'

  type Contact = {
    peerId: string
    name: string
    online: boolean
    secure: boolean
    unread: number
    fingerprint: string
  }

  type LinkedDevice = {
    peerId: string
    name: string
    online: boolean
    lastSeenMs: number
  }

  type ClientMessage = {
    peerId: string
    direction: 'in' | 'out'
    kind: string
    body: string
    timestampMs: number
    emoticons: ClientEmoticonSpan[]
    attachmentId?: string
    attachmentMime?: string
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
    attachmentId?: string
    attachmentMime?: string
    attachmentDataUrl?: string
    senderPeerId?: string
  }

  type GroupChat = {
    id: string
    name: string
    ownerPeerId: string
    members: string[]
    admins: string[]
    silenced: string[]
    bans: GroupBan[]
    unread: number
  }

  type GroupBan = { peerId: string; expiresAtMs: number | null }

  type IncomingAttachmentOffer = {
    offerId: number
    peerId: string
    filename: string
    mime: string
    size: number
    groupId?: string
  }

  type ClientGroupMessage = {
    groupId: string
    senderPeerId: string
    direction: 'in' | 'out'
    kind: string
    body: string
    timestampMs: number
    emoticons: ClientEmoticonSpan[]
    attachmentId?: string
    attachmentMime?: string
  }

  type ClientEvent =
    | { type: 'started'; peerId: string; displayName: string; fingerprint: string }
    | { type: 'contactUpdated'; contact: Omit<Contact, 'unread'> }
    | { type: 'conversationLoaded'; peerId: string; messages: ClientMessage[] }
    | { type: 'message'; message: ClientMessage }
    | { type: 'emoticonCatalog'; emoticons: ClientEmoticon[] }
    | { type: 'emoticonOffered'; peerId: string; emoticon: ClientEmoticon }
    | { type: 'emoticonRemoved'; assetId: string }
    | { type: 'contactLink'; link: string; qrLink: string }
    | { type: 'deviceLink'; link: string; qrLink: string; expiresAtMs: number }
    | { type: 'devicesUpdated'; devices: LinkedDevice[] }
    | { type: 'deviceSynchronized'; peerId: string; applied: number; paired: boolean }
    | { type: 'attachmentReceived'; peerId: string; id: string; filename: string; mime: string }
    | { type: 'groupAttachmentReceived'; groupId: string; id: string; filename: string; mime: string }
    | { type: 'attachmentSent'; peerId: string; filename: string }
    | { type: 'attachmentProgress'; filename: string; completedChunks: number; totalChunks: number }
    | { type: 'attachmentTransfersCancelled' }
    | ({ type: 'incomingAttachmentOffered' } & IncomingAttachmentOffer)
    | { type: 'contactRemoved'; peerId: string }
    | { type: 'conversationCleared'; peerId: string }
    | { type: 'groupChatsUpdated'; groups: Omit<GroupChat, 'unread'>[] }
    | { type: 'groupConversationLoaded'; groupId: string; messages: ClientGroupMessage[] }
    | { type: 'groupMessage'; message: ClientGroupMessage }
    | { type: 'groupConversationCleared'; groupId: string }
    | { type: 'attachmentOpened'; id: string; dataUrl: string }
    | { type: 'attachmentExported'; path: string }
    | { type: 'error'; message: string }
    | { type: 'ready' }
    | { type: 'stopped' }

  type Theme = 'light' | 'dark' | 'system'
  type SettingsSection = 'profile' | 'appearance' | 'devices' | 'data' | 'updates' | 'network'
  type UpdateStatus = 'idle' | 'checking' | 'available' | 'downloading' | 'installing' | 'current' | 'error'
  type Emoticon = { glyph: string; shortcut: string; label: string }
  type MessagePart = { text: string; emoticon?: Emoticon; custom?: ClientEmoticon }
  type Profile = {
    name: string
    avatarDataUrl: string | null
    previewSentImages: boolean
    previewReceivedImages: boolean
    nudgeSound: boolean
    relayAddress: string
    fontScale: number
  }

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

  const appWindow = isTauri() ? getCurrentWindow() : null
  const securityIntroKey = 'msnnext-security-intro-v1'
  const notificationMutesKey = 'msnnext-notification-mutes-v1'
  const lastUpdateCheckKey = 'msnnext-update-last-check-v1'
  const updateCheckIntervalMs = 5 * 60 * 60 * 1000

  function loadNotificationMutes() {
    if (typeof localStorage === 'undefined') return {} as Record<string, number>
    try {
      const parsed = JSON.parse(localStorage.getItem(notificationMutesKey) || '{}') as Record<string, unknown>
      return Object.fromEntries(Object.entries(parsed).filter(([, until]) =>
        typeof until === 'number' && (until === -1 || until > Date.now())
      )) as Record<string, number>
    } catch {
      return {} as Record<string, number>
    }
  }

  function dragWindow(event: MouseEvent) {
    if (event.button === 0 && !(event.target as HTMLElement).closest('button')) void appWindow?.startDragging()
  }

  function maximizeWindow(event: MouseEvent) {
    if (!(event.target as HTMLElement).closest('button')) void appWindow?.toggleMaximize()
  }

  const savedTheme = typeof localStorage === 'undefined' ? null : localStorage.getItem('msnnext-theme')
  let theme: Theme = savedTheme === 'light' || savedTheme === 'dark' || savedTheme === 'system'
    ? savedTheme
    : 'system'
  let displayName = typeof localStorage === 'undefined'
    ? 'Amico'
    : localStorage.getItem('msnnext-name') || 'Amico'
  let directAddress = ''
  let relayAddress = ''
  let searchQuery = ''
  let messageText = ''
  let contactLink = ''
  let ownContactLink = ''
  let ownContactQr = ''
  let peerId = ''
  let ownFingerprint = ''
  let selectedPeerId = ''
  let selectedGroupId = ''
  let contacts: Contact[] = []
  let conversations: Record<string, ChatMessage[]> = {}
  let customEmoticons: ClientEmoticon[] = []
  let offeredEmoticons: ClientEmoticon[] = []
  let running = false
  let starting = false
  let setupOpen = true
  let profileOpen = false
  let settingsSection: SettingsSection = 'profile'
  let accountBackupOpen = false
  let accountBackupMode: 'export' | 'import' = 'export'
  let accountBackupPath = ''
  let accountBackupPassword = ''
  let accountBackupBusy = false
  let devicePairingOpen = false
  let devicePairingMode: 'share' | 'join' = 'share'
  let devicePairingLink = ''
  let devicePairingQr = ''
  let devicePairingExpiresAt = 0
  let devicePairingBusy = false
  let linkedDevices: LinkedDevice[] = []
  let securityIntroOpen = typeof localStorage !== 'undefined'
    && localStorage.getItem(securityIntroKey) !== 'seen'
  let avatarDataUrl = ''
  let previewSentImages = true
  let previewReceivedImages = false
  let nudgeSound = true
  let fontScale = 125
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
  let pendingFileCount = 0
  let transferFilename = ''
  let transferProgress = 0
  let incomingAttachmentOffers: IncomingAttachmentOffer[] = []
  let contactName = ''
  let contactNamePeer = ''
  let chatGroups: GroupChat[] = []
  let groupCreateOpen = false
  let pendingGroupCreation = false
  let groupName = ''
  let groupMemberIds: string[] = []
  let contextPeerId = ''
  let contextGroupId = ''
  let contextX = 0
  let contextY = 0
  let fileDropActive = false
  let mediaPreview = ''
  let pendingSentPreviews: Record<string, string[]> = {}
  const automaticPreviewIds = new Set<string>()
  let rosterOpen = false
  let linkRequested = false
  let toastText = ''
  let toastTimer: ReturnType<typeof setTimeout>
  let messageList: HTMLDivElement
  let messageEditor: HTMLDivElement
  let windowFocused = true
  let notificationMutes = loadNotificationMutes()
  let overlayIcon: Image | undefined
  let taskbarUpdate = 0
  let appVersion = '0.2.0'
  let updateCandidate: Update | null = null
  let updateStatus: UpdateStatus = 'idle'
  let updateMessage = ''
  let updateProgress = 0
  let updateDownloaded = 0
  let updateDownloadTotal = 0
  let lastUpdateCheck = typeof localStorage === 'undefined'
    ? 0
    : Number(localStorage.getItem(lastUpdateCheckKey) || 0)

  $: activeContact = contacts.find((contact) => contact.peerId === selectedPeerId)
  $: activeGroup = chatGroups.find((group) => group.id === selectedGroupId)
  $: conversationKey = selectedGroupId ? `group:${selectedGroupId}` : selectedPeerId
  $: messages = conversationKey ? conversations[conversationKey] || [] : []
  $: visibleContacts = contacts.filter((contact) =>
    contact.name.toLocaleLowerCase().includes(searchQuery.trim().toLocaleLowerCase())
  )
  $: onlineContacts = visibleContacts.filter((contact) => contact.online)
  $: offlineContacts = visibleContacts.filter((contact) => !contact.online)
  $: sortedContacts = [...visibleContacts].sort((a, b) => Number(b.online) - Number(a.online) || a.name.localeCompare(b.name))
  $: groupOnline = activeGroup?.members.filter((id) =>
    id !== peerId
    && !memberBan(activeGroup, id)
    && contacts.some((contact) => contact.peerId === id && contact.secure)
  ).length || 0
  $: ready = activeGroup ? groupOnline > 0 : Boolean(activeContact?.online && activeContact?.secure)
  $: groupCanSend = !activeGroup
    || (!activeGroup.silenced.includes(peerId) && !memberBan(activeGroup, peerId))
  $: canSend = ready && groupCanSend
  $: totalUnread = contacts.reduce((total, contact) => total + contact.unread, 0)
    + chatGroups.reduce((total, group) => total + group.unread, 0)
  $: void updateTaskbarBadge(totalUnread)
  $: if (typeof document !== 'undefined') document.documentElement.dataset.fontScale = String(fontScale)

  onMount(() => {
    const media = window.matchMedia('(prefers-color-scheme: dark)')
    const syncSystemTheme = () => {
      if (theme === 'system') applyTheme()
    }
    media.addEventListener('change', syncSystemTheme)
    applyTheme()

    let unlisten: UnlistenFn | undefined
    let unlistenDrop: UnlistenFn | undefined
    let unlistenFocus: UnlistenFn | undefined
    let updateTimer: ReturnType<typeof setTimeout> | undefined
    const blockNativeMenu = (event: MouseEvent) => event.preventDefault()
    const openPreferences = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key === ',') {
        event.preventDefault()
        openSettings()
      }
    }
    window.addEventListener('contextmenu', blockNativeMenu)
    window.addEventListener('keydown', openPreferences)
    if (isTauri()) {
      void getVersion().then((version) => appVersion = version)
      void initializeApp().then((stop) => unlisten = stop)
      const scheduleUpdateCheck = () => {
        const elapsed = Date.now() - lastUpdateCheck
        const delay = Math.max(1_000, updateCheckIntervalMs - Math.max(0, elapsed))
        updateTimer = setTimeout(async () => {
          await checkForUpdates(false)
          scheduleUpdateCheck()
        }, delay)
      }
      scheduleUpdateCheck()
    }
    if (appWindow) {
      void appWindow.isFocused().then((focused) => windowFocused = focused)
      void appWindow.onFocusChanged(({ payload: focused }) => {
        windowFocused = focused
        if (focused) {
          markActiveConversationRead()
          void appWindow.requestUserAttention(null)
        }
      }).then((stop) => unlistenFocus = stop)
    }
    if (isTauri()) void getCurrentWebview().onDragDropEvent((event) => {
      fileDropActive = event.payload.type === 'enter' || event.payload.type === 'over'
      if (event.payload.type === 'drop') void sendFiles(event.payload.paths)
      if (event.payload.type === 'drop' || event.payload.type === 'leave') fileDropActive = false
    }).then((stop) => unlistenDrop = stop)

    return () => {
      unlisten?.()
      unlistenDrop?.()
      unlistenFocus?.()
      if (updateTimer) clearTimeout(updateTimer)
      window.removeEventListener('contextmenu', blockNativeMenu)
      window.removeEventListener('keydown', openPreferences)
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
      previewSentImages = profile.previewSentImages
      previewReceivedImages = profile.previewReceivedImages
      nudgeSound = profile.nudgeSound
      relayAddress = profile.relayAddress
      fontScale = profile.fontScale
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

  function openSettings(section: SettingsSection = 'profile') {
    settingsSection = section
    profileOpen = true
  }

  async function checkForUpdates(force: boolean) {
    if (!isTauri() || updateStatus === 'checking' || updateStatus === 'downloading' || updateStatus === 'installing') return
    if (!force && Date.now() - lastUpdateCheck < updateCheckIntervalMs) return

    lastUpdateCheck = Date.now()
    localStorage.setItem(lastUpdateCheckKey, String(lastUpdateCheck))
    updateStatus = 'checking'
    updateMessage = 'Controllo della versione disponibile…'
    try {
      const update = await check({ timeout: 30_000 })
      if (!update) {
        await updateCandidate?.close()
        updateCandidate = null
        updateStatus = 'current'
        updateMessage = 'Stai usando la versione più recente.'
        if (force) showToast('msnnext è aggiornato')
        return
      }

      await updateCandidate?.close()
      updateCandidate = update
      updateStatus = 'available'
      updateMessage = `La versione ${update.version} è pronta per l'installazione.`
      if (force) showToast(`Aggiornamento ${update.version} disponibile`)
    } catch (error) {
      updateStatus = 'error'
      updateMessage = `Controllo non riuscito: ${String(error)}`
      if (force) showToast('Impossibile controllare gli aggiornamenti')
      else console.warn('Controllo aggiornamenti non riuscito', error)
    }
  }

  async function installUpdate() {
    if (!updateCandidate || updateStatus === 'downloading' || updateStatus === 'installing') return
    updateStatus = 'downloading'
    updateProgress = 0
    updateDownloaded = 0
    updateDownloadTotal = 0
    updateMessage = `Download di msnnext ${updateCandidate.version}…`
    try {
      await updateCandidate.downloadAndInstall((event) => {
        if (event.event === 'Started') {
          updateDownloadTotal = event.data.contentLength || 0
          return
        }
        if (event.event === 'Progress') {
          updateDownloaded += event.data.chunkLength
          updateProgress = updateDownloadTotal
            ? Math.min(99, Math.round(updateDownloaded * 100 / updateDownloadTotal))
            : 0
          return
        }
        updateProgress = 100
        updateStatus = 'installing'
        updateMessage = 'Installazione completata. Riavvio di msnnext…'
      })
      await relaunch()
    } catch (error) {
      updateStatus = 'error'
      updateMessage = `Aggiornamento non riuscito: ${String(error)}`
      showToast('Aggiornamento non riuscito')
    }
  }

  function lastUpdateCheckLabel() {
    if (!lastUpdateCheck) return 'Non ancora controllato'
    return new Date(lastUpdateCheck).toLocaleString([], {
      day: '2-digit', month: 'short', hour: '2-digit', minute: '2-digit',
    })
  }

  function peerConversationKey(peer: string) {
    return `peer:${peer}`
  }

  function groupConversationKey(group: string) {
    return `group:${group}`
  }

  function isConversationMuted(conversation: string) {
    const until = notificationMutes[conversation]
    return until === -1 || until > Date.now()
  }

  function markActiveConversationRead() {
    if (selectedPeerId) {
      contacts = contacts.map((contact) =>
        contact.peerId === selectedPeerId ? { ...contact, unread: 0 } : contact
      )
    } else if (selectedGroupId) {
      chatGroups = chatGroups.map((group) =>
        group.id === selectedGroupId ? { ...group, unread: 0 } : group
      )
    }
  }

  async function muteConversation(conversation: string, durationMs: number | null) {
    const until = durationMs === null ? -1 : Date.now() + durationMs
    notificationMutes = { ...notificationMutes, [conversation]: until }
    localStorage.setItem(notificationMutesKey, JSON.stringify(notificationMutes))
    closeContextMenu()
    if (running) await invoke('node_set_notification_mute', {
      conversation,
      muted: true,
      untilMs: until === -1 ? null : until,
    }).catch((error) => showToast(String(error)))
    showToast(durationMs === null ? 'Chat silenziata' : 'Chat silenziata temporaneamente')
  }

  async function unmuteConversation(conversation: string) {
    const { [conversation]: _removed, ...remaining } = notificationMutes
    notificationMutes = remaining
    localStorage.setItem(notificationMutesKey, JSON.stringify(notificationMutes))
    closeContextMenu()
    if (running) await invoke('node_set_notification_mute', {
      conversation, muted: false, untilMs: null,
    }).catch((error) => showToast(String(error)))
    showToast('Notifiche riattivate')
  }

  async function syncNotificationMutes() {
    for (const [conversation, until] of Object.entries(notificationMutes)) {
      await invoke('node_set_notification_mute', {
        conversation,
        muted: true,
        untilMs: until === -1 ? null : until,
      })
    }
  }

  function notifyTaskbar(conversation: string) {
    if (windowFocused || isConversationMuted(conversation)) return
    void appWindow?.requestUserAttention(UserAttentionType.Informational)
  }

  function unreadOverlayPixels(count: number) {
    const canvas = document.createElement('canvas')
    canvas.width = 32
    canvas.height = 32
    const context = canvas.getContext('2d')!
    context.fillStyle = '#0872b3'
    context.beginPath()
    context.arc(16, 16, 15, 0, Math.PI * 2)
    context.fill()
    context.fillStyle = '#ffffff'
    context.font = `700 ${count > 99 ? 12 : count > 9 ? 15 : 20}px "Segoe UI"`
    context.textAlign = 'center'
    context.textBaseline = 'middle'
    context.fillText(count > 99 ? '99+' : String(count), 16, 16)
    return new Uint8Array(context.getImageData(0, 0, 32, 32).data)
  }

  async function updateTaskbarBadge(count: number) {
    if (!appWindow) return
    const update = ++taskbarUpdate
    try {
      if (/Windows/i.test(navigator.userAgent)) {
        const nextIcon = count ? await Image.new(unreadOverlayPixels(count), 32, 32) : undefined
        if (update !== taskbarUpdate) {
          await nextIcon?.close()
          return
        }
        await appWindow.setOverlayIcon(nextIcon)
        await overlayIcon?.close()
        overlayIcon = nextIcon
      } else {
        await appWindow.setBadgeCount(count || undefined)
      }
    } catch (error) {
      console.warn('Badge taskbar non aggiornabile', error)
    }
  }

  function closeSecurityIntro() {
    localStorage.setItem(securityIntroKey, 'seen')
    securityIntroOpen = false
  }

  function handleEvent(event: ClientEvent) {
    if (event.type === 'started') {
      peerId = event.peerId
      ownFingerprint = event.fingerprint
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
      void QRCode.toDataURL(event.qrLink, {
        width: 1024,
        margin: 4,
        color: { dark: '#10284a', light: '#ffffff' },
      }).then((qr) => ownContactQr = qr).catch((error) => showToast(`QR non generabile: ${error}`))
      return
    }
    if (event.type === 'deviceLink') {
      devicePairingLink = event.link
      devicePairingExpiresAt = event.expiresAtMs
      devicePairingBusy = false
      void QRCode.toDataURL(event.qrLink, {
        width: 1024,
        margin: 4,
        color: { dark: '#10284a', light: '#ffffff' },
      }).then((qr) => devicePairingQr = qr).catch((error) => showToast(`QR non generabile: ${error}`))
      return
    }
    if (event.type === 'devicesUpdated') {
      linkedDevices = event.devices
      return
    }
    if (event.type === 'deviceSynchronized') {
      devicePairingBusy = false
      if (event.paired) {
        devicePairingOpen = false
        showToast('Dispositivo collegato')
      } else if (event.applied) {
        showToast(`${event.applied} modifiche sincronizzate`)
      }
      return
    }
    if (event.type === 'attachmentReceived') {
      const conversation = [...(conversations[event.peerId] || [])]
      const index = conversation.findLastIndex((message) => !message.mine && message.kind === 'file' && message.body === event.filename)
      if (index >= 0) conversation[index] = {
        ...conversation[index], attachmentId: event.id, attachmentMime: event.mime,
      }
      conversations = { ...conversations, [event.peerId]: conversation }
      showToast(`File ricevuto: ${event.filename}`)
      if (previewReceivedImages && event.mime.startsWith('image/')) {
        automaticPreviewIds.add(event.id)
        void invoke('node_read_attachment', { id: event.id, mime: event.mime }).catch((error) => {
          automaticPreviewIds.delete(event.id)
          showToast(String(error))
        })
      }
      return
    }
    if (event.type === 'attachmentSent') {
      pendingFileCount = Math.max(0, pendingFileCount - 1)
      fileSending = pendingFileCount > 0
      showToast(`File inviato: ${event.filename}`)
      return
    }
    if (event.type === 'attachmentProgress') {
      transferFilename = event.filename
      transferProgress = event.totalChunks
        ? Math.round(event.completedChunks * 100 / event.totalChunks)
        : 100
      return
    }
    if (event.type === 'attachmentTransfersCancelled') {
      pendingFileCount = 0
      fileSending = false
      transferFilename = ''
      transferProgress = 0
      showToast('Invio annullato')
      return
    }
    if (event.type === 'incomingAttachmentOffered') {
      incomingAttachmentOffers = [...incomingAttachmentOffers, event]
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
    if (event.type === 'groupChatsUpdated') {
      chatGroups = event.groups.map((group) => ({
        ...group,
        unread: chatGroups.find((current) => current.id === group.id)?.unread || 0,
      }))
      if (selectedGroupId && !chatGroups.some((group) => group.id === selectedGroupId)) {
        selectedGroupId = ''
      }
      return
    }
    if (event.type === 'groupConversationLoaded') {
      conversations = {
        ...conversations,
        [`group:${event.groupId}`]: event.messages.map(toGroupChatMessage),
      }
      if (pendingGroupCreation) {
        pendingGroupCreation = false
        groupCreateOpen = false
        selectGroup(event.groupId)
        showToast('Chat di gruppo creata')
      }
      scrollMessages()
      return
    }
    if (event.type === 'groupMessage') {
      const key = `group:${event.message.groupId}`
      const next = toGroupChatMessage(event.message)
      if (event.message.direction === 'out' && event.message.kind === 'file') {
        consumeSentPreview(next, key, event.message.body)
      }
      conversations = {
        ...conversations,
        [key]: [...(conversations[key] || []), next],
      }
      if (event.message.direction === 'in' && (!windowFocused || selectedGroupId !== event.message.groupId)) {
        chatGroups = chatGroups.map((group) => group.id === event.message.groupId
          ? { ...group, unread: group.unread + 1 }
          : group)
        notifyTaskbar(groupConversationKey(event.message.groupId))
      }
      scrollMessages()
      return
    }
    if (event.type === 'groupAttachmentReceived') {
      const key = `group:${event.groupId}`
      const conversation = [...(conversations[key] || [])]
      const index = conversation.findLastIndex((message) => !message.mine && message.kind === 'file' && message.body === event.filename)
      if (index >= 0) conversation[index] = {
        ...conversation[index], attachmentId: event.id, attachmentMime: event.mime,
      }
      conversations = { ...conversations, [key]: conversation }
      showToast(`File ricevuto nel gruppo: ${event.filename}`)
      if (previewReceivedImages && event.mime.startsWith('image/')) {
        automaticPreviewIds.add(event.id)
        void invoke('node_read_attachment', { id: event.id, mime: event.mime }).catch((error) => {
          automaticPreviewIds.delete(event.id)
          showToast(String(error))
        })
      }
      return
    }
    if (event.type === 'groupConversationCleared') {
      conversations = { ...conversations, [`group:${event.groupId}`]: [] }
      showToast('Cronologia del gruppo eliminata')
      return
    }
    if (event.type === 'attachmentOpened') {
      for (const [id, conversation] of Object.entries(conversations)) {
        conversations = {
          ...conversations,
          [id]: conversation.map((message) => message.attachmentId === event.id
            ? { ...message, attachmentDataUrl: event.dataUrl }
            : message),
        }
      }
      if (automaticPreviewIds.has(event.id)) automaticPreviewIds.delete(event.id)
      else mediaPreview = event.dataUrl
      return
    }
    if (event.type === 'attachmentExported') {
      showToast(`File esportato: ${event.path}`)
      return
    }
    if (event.type === 'error') {
      linkRequested = false
      fileSending = false
      pendingFileCount = 0
      pendingEmoticonAction = ''
      pendingGroupCreation = false
      devicePairingBusy = false
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
        contact.peerId === next.peerId
          ? { ...contact, ...next }
          : contact
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
      attachmentId: message.attachmentId,
      attachmentMime: message.attachmentMime,
      mine: message.direction === 'out',
      time: new Intl.DateTimeFormat('it', {
        hour: '2-digit',
        minute: '2-digit',
      }).format(new Date(message.timestampMs)),
    }
  }

  function toGroupChatMessage(message: ClientGroupMessage): ChatMessage {
    return {
      id: `${message.timestampMs}-${crypto.randomUUID()}`,
      kind: message.kind === 'file' ? 'file' : message.direction === 'out' ? 'outgoing' : 'incoming',
      body: message.body,
      emoticons: message.emoticons || [],
      mine: message.direction === 'out',
      senderPeerId: message.senderPeerId,
      attachmentId: message.attachmentId,
      attachmentMime: message.attachmentMime,
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
    const next = toChatMessage(message)
    if (message.direction === 'out' && message.kind === 'file') {
      consumeSentPreview(next, message.peerId, message.body)
    }
    conversations = {
      ...conversations,
      [message.peerId]: [...conversation, next],
    }
    const conversationKey = peerConversationKey(message.peerId)
    if (message.direction === 'in' && (!windowFocused || selectedPeerId !== message.peerId)) {
      contacts = contacts.map((contact) =>
        contact.peerId === message.peerId
          ? { ...contact, unread: contact.unread + 1 }
          : contact
      )
      notifyTaskbar(conversationKey)
    }
    if (message.kind === 'nudge' && !next.mine && !isConversationMuted(conversationKey)) {
      void shakeWindow()
      playNudgeSound()
    }
    scrollMessages()
  }

  function consumeSentPreview(message: ChatMessage, conversation: string, filename: string) {
    const key = `${conversation}\u0000${filename}`
    const previews = pendingSentPreviews[key]
    if (!previews?.length) return
    message.attachmentDataUrl = previews.shift()
    if (!previews.length) delete pendingSentPreviews[key]
  }

  function selectContact(id: string) {
    selectedPeerId = id
    selectedGroupId = ''
    contactName = contacts.find((contact) => contact.peerId === id)?.name || ''
    contactNamePeer = id
    rosterOpen = false
    contacts = contacts.map((contact) =>
      contact.peerId === id ? { ...contact, unread: 0 } : contact
    )
    scrollMessages()
  }

  function selectGroup(id: string) {
    selectedGroupId = id
    selectedPeerId = ''
    detailsOpen = false
    rosterOpen = false
    chatGroups = chatGroups.map((group) => group.id === id ? { ...group, unread: 0 } : group)
    scrollMessages()
  }

  function senderName(message: ChatMessage) {
    if (message.mine) return displayName
    if (!activeGroup) return activeContact?.name || 'Contatto'
    return contacts.find((contact) => contact.peerId === message.senderPeerId)?.name
      || `${message.senderPeerId?.slice(0, 8) || 'Partecipante'}…`
  }

  function memberName(memberId: string) {
    if (memberId === peerId) return `${displayName} (tu)`
    return contacts.find((contact) => contact.peerId === memberId)?.name || `${memberId.slice(0, 8)}…`
  }

  function memberBan(group: GroupChat, memberId: string) {
    return group.bans.find((ban) => ban.peerId === memberId
      && (ban.expiresAtMs === null || ban.expiresAtMs > Date.now()))
  }

  function memberRole(group: GroupChat, memberId: string) {
    if (memberId === group.ownerPeerId) return 'Proprietario'
    if (group.admins.includes(memberId)) return 'Amministratore'
    return 'Membro'
  }

  function canModerateMember(group: GroupChat, memberId: string) {
    if (memberId === peerId || memberId === group.ownerPeerId) return false
    if (peerId === group.ownerPeerId) return true
    return group.admins.includes(peerId) && !group.admins.includes(memberId)
  }

  function banLabel(ban: GroupBan) {
    if (ban.expiresAtMs === null) return 'Ban permanente'
    return `Ban fino al ${new Date(ban.expiresAtMs).toLocaleString()}`
  }

  function deviceStatus(device: LinkedDevice) {
    if (device.online) return 'Online, sincronizzazione attiva'
    if (!device.lastSeenMs) return 'Mai collegato'
    return `Ultimo collegamento ${new Date(device.lastSeenMs).toLocaleString()}`
  }

  async function moderateGroup(memberId: string, value: string) {
    if (!selectedGroupId || !value) return
    const [action, duration] = value.split(':')
    if (action === 'permaBan' && !confirm(`Bannare definitivamente ${memberName(memberId)}?`)) return
    await invoke('node_moderate_group', {
      groupId: selectedGroupId,
      peerId: memberId,
      action,
      durationMs: duration ? Number(duration) : null,
    }).catch((error) => showToast(String(error)))
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

  function messageParts(message: ChatMessage, availableEmoticons: ClientEmoticon[]): MessagePart[] {
    if (!message.emoticons.length) return builtinMessageParts(message.body)
    const parts: MessagePart[] = []
    let cursor = 0
    for (const span of [...message.emoticons].sort((a, b) => a.start - b.start)) {
      const start = textIndexAtByteOffset(message.body, span.start)
      const end = textIndexAtByteOffset(message.body, span.end)
      if (start < cursor || end <= start) continue
      parts.push(...builtinMessageParts(message.body.slice(cursor, start)))
      const custom = availableEmoticons.find((item) => item.assetId === span.assetId)
      parts.push(custom
        ? { text: message.body.slice(start, end), custom }
        : { text: message.body.slice(start, end) })
      cursor = end
    }
    parts.push(...builtinMessageParts(message.body.slice(cursor)))
    return parts
  }

  function draftText(node: Node): string {
    if (node.nodeType === Node.TEXT_NODE) return node.textContent || ''
    if (node instanceof HTMLElement && node.dataset.trigger) return node.dataset.trigger
    if (node instanceof HTMLBRElement) return '\n'
    return Array.from(node.childNodes).map(draftText).join('')
  }

  function insertAtDraftCaret(text: string) {
    messageEditor.focus()
    const selection = window.getSelection()
    const range = document.createRange()
    if (selection?.anchorNode && messageEditor.contains(selection.anchorNode)) {
      range.setStart(selection.anchorNode, selection.anchorOffset)
    } else {
      range.selectNodeContents(messageEditor)
      range.collapse(false)
    }
    range.collapse(true)
    const node = document.createTextNode(text)
    range.insertNode(node)
    range.setStart(node, text.length)
    range.collapse(true)
    selection?.removeAllRanges()
    selection?.addRange(range)
  }

  function decorateDraftAtCaret() {
    const selection = window.getSelection()
    const node = selection?.anchorNode
    if (!(node instanceof Text) || !messageEditor.contains(node)) return
    const offset = selection?.anchorOffset || 0
    const prefix = node.data.slice(0, offset)
    const match = [
      ...customEmoticons.map((item) => ({ trigger: item.trigger, item })),
      ...emoticons.map((item) => ({ trigger: item.shortcut, item })),
    ].sort((a, b) => b.trigger.length - a.trigger.length)
      .find(({ trigger }) => prefix.endsWith(trigger))
    if (!match) return

    const token = match.item && 'dataUrl' in match.item
      ? document.createElement('img')
      : document.createElement('span')
    token.dataset.trigger = match.trigger
    token.className = 'draft-emoticon'
    token.contentEditable = 'false'
    if (token instanceof HTMLImageElement && 'dataUrl' in match.item) {
      token.src = match.item.dataUrl
      token.alt = match.item.name
    } else {
      token.textContent = 'glyph' in match.item ? match.item.glyph : match.trigger
    }
    const range = document.createRange()
    range.setStart(node, offset - match.trigger.length)
    range.setEnd(node, offset)
    range.deleteContents()
    range.insertNode(token)
    range.setStartAfter(token)
    range.collapse(true)
    selection?.removeAllRanges()
    selection?.addRange(range)
  }

  function syncDraft() {
    decorateDraftAtCaret()
    messageText = draftText(messageEditor).slice(0, 4000)
    if (!messageText.trim()) {
      messageText = ''
      messageEditor.replaceChildren()
    }
  }

  function insertDraftTrigger(trigger: string) {
    if (!canSend) return
    insertAtDraftCaret(`${messageText && !messageText.endsWith(' ') ? ' ' : ''}${trigger}`)
    decorateDraftAtCaret()
    insertAtDraftCaret(' ')
    syncDraft()
  }

  function insertEmoticon(item: Emoticon) {
    insertDraftTrigger(item.shortcut)
  }

  function insertCustomEmoticon(item: ClientEmoticon) {
    insertDraftTrigger(item.trigger)
  }

  function pasteDraft(event: ClipboardEvent) {
    event.preventDefault()
    const remaining = Math.max(0, 4000 - messageText.length)
    insertAtDraftCaret((event.clipboardData?.getData('text/plain') || '').slice(0, remaining))
    syncDraft()
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
          previewSentImages, previewReceivedImages, nudgeSound, fontScale,
          relayAddress,
        })
        avatarDataUrl = profile.avatarDataUrl || ''
      }
      await invoke('node_start', {
        config: {
          name: displayName.trim(),
          connect: directAddress.trim() || null,
          relay: relayAddress.trim() || null,
        },
      })
      await syncNotificationMutes()
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
    if (!text || !canSend || (!selectedPeerId && !selectedGroupId)) return
    try {
      if (selectedGroupId) await invoke('node_send_group_text', { groupId: selectedGroupId, text })
      else await invoke('node_send_text', { peerId: selectedPeerId, text })
      messageText = ''
      messageEditor.replaceChildren()
      emojiOpen = false
    } catch (error) {
      showToast(String(error))
    }
  }

  async function sendNudge() {
    if (!ready || !selectedPeerId || selectedGroupId) return
    try {
      await invoke('node_send_nudge', { peerId: selectedPeerId })
    } catch (error) {
      showToast(String(error))
    }
  }

  async function chooseFile() {
    if (!canSend || (!selectedPeerId && !selectedGroupId)) return
    const selected = await open({ multiple: false, directory: false })
    if (!selected || Array.isArray(selected)) return
    await sendFiles([selected])
  }

  async function cancelFileTransfers() {
    await invoke('node_cancel_file_transfers').catch((error) => showToast(String(error)))
  }

  async function answerAttachmentOffer(offer: IncomingAttachmentOffer, accept: boolean) {
    try {
      await invoke(accept ? 'node_accept_attachment' : 'node_reject_attachment', {
        offerId: offer.offerId,
      })
      incomingAttachmentOffers = incomingAttachmentOffers.filter((item) => item.offerId !== offer.offerId)
    } catch (error) {
      showToast(String(error))
    }
  }

  function formatBytes(bytes: number) {
    if (bytes < 1024) return `${bytes} B`
    if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`
    if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`
    return `${(bytes / 1024 ** 3).toFixed(2)} GB`
  }

  async function sendFiles(paths: string[]) {
    if (!canSend || (!selectedPeerId && !selectedGroupId) || !paths.length) return
    const targetPeer = selectedPeerId
    const targetGroup = selectedGroupId
    const targetConversation = targetGroup ? `group:${targetGroup}` : targetPeer
    const targetCount = targetGroup ? Math.max(1, groupOnline) : 1
    pendingFileCount += paths.length * targetCount
    fileSending = true
    for (const path of paths) {
      try {
        const filename = path.split(/[\\/]/).at(-1) || path
        if (previewSentImages && isImagePath(path)) {
          try {
            const preview = await invoke<string>('image_preview', { path })
            const key = `${targetConversation}\u0000${filename}`
            pendingSentPreviews[key] = [...(pendingSentPreviews[key] || []), preview]
          } catch {
            // L'anteprima è facoltativa: il file può comunque essere inviato.
          }
        }
        if (targetGroup) await invoke('node_send_group_file', { groupId: targetGroup, path })
        else await invoke('node_send_file', { peerId: targetPeer, path })
      } catch (error) {
        pendingFileCount = Math.max(0, pendingFileCount - targetCount)
        fileSending = pendingFileCount > 0
        showToast(String(error))
      }
    }
  }

  function isImagePath(path: string) {
    return /\.(avif|bmp|gif|jpe?g|png|webp)$/i.test(path)
  }

  function showContactMenu(event: MouseEvent, contact: Contact) {
    event.preventDefault()
    contextPeerId = contact.peerId
    contextGroupId = ''
    contextX = Math.min(event.clientX, window.innerWidth - 230)
    contextY = Math.min(event.clientY, window.innerHeight - 285)
  }

  function showGroupMenu(event: MouseEvent, group: GroupChat) {
    event.preventDefault()
    contextGroupId = group.id
    contextPeerId = ''
    contextX = Math.min(event.clientX, window.innerWidth - 230)
    contextY = Math.min(event.clientY, window.innerHeight - 255)
  }

  function closeContextMenu() {
    contextPeerId = ''
    contextGroupId = ''
  }

  function manageContact(peerId: string) {
    selectContact(peerId)
    contactName = contacts.find((contact) => contact.peerId === peerId)?.name || ''
    contactNamePeer = peerId
    detailsOpen = true
    closeContextMenu()
  }

  function openGroupCreation() {
    groupName = ''
    groupMemberIds = []
    groupCreateOpen = true
  }

  function toggleGroupMember(peerId: string) {
    groupMemberIds = groupMemberIds.includes(peerId)
      ? groupMemberIds.filter((id) => id !== peerId)
      : [...groupMemberIds, peerId]
  }

  async function createChatGroup() {
    if (!groupName.trim() || groupMemberIds.length < 2) return
    try {
      pendingGroupCreation = true
      await invoke('node_create_chat_group', { name: groupName.trim(), members: groupMemberIds })
    } catch (error) {
      pendingGroupCreation = false
      showToast(String(error))
    }
  }

  async function clearGroupConversation() {
    if (!selectedGroupId || !confirm('Eliminare la cronologia di questa chat di gruppo?')) return
    await invoke('node_clear_group_conversation', { groupId: selectedGroupId })
      .catch((error) => showToast(String(error)))
  }

  async function deleteChatGroup() {
    if (!selectedGroupId || !confirm('Rimuovere questa chat di gruppo dal dispositivo?')) return
    const id = selectedGroupId
    selectedGroupId = ''
    await invoke('node_delete_chat_group', { groupId: id })
      .catch((error) => showToast(String(error)))
  }

  async function openAttachment(message: ChatMessage) {
    if (!message.attachmentId || !message.attachmentMime) {
      showToast('Il file appartiene a una vecchia cronologia e non ha un riferimento all’archivio')
      return
    }
    if (message.attachmentMime.startsWith('image/') || message.attachmentMime.startsWith('video/')) {
      await invoke('node_read_attachment', {
        id: message.attachmentId, mime: message.attachmentMime,
      }).catch((error) => showToast(String(error)))
      return
    }
    const path = await save({ defaultPath: message.body })
    if (path) await invoke('node_export_attachment', {
      id: message.attachmentId, path,
    }).catch((error) => showToast(String(error)))
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

  async function saveProfile(
    avatarPath: string | null = null,
    clearAvatar = false,
    closeAfterSave = true,
  ) {
    if (!displayName.trim()) return
    try {
      const profile = await invoke<Profile>('profile_save', {
        name: displayName.trim(), avatarPath, clearAvatar,
        previewSentImages, previewReceivedImages, nudgeSound, fontScale,
        relayAddress,
      })
      displayName = profile.name
      avatarDataUrl = profile.avatarDataUrl || ''
      localStorage.setItem('msnnext-name', profile.name)
      if (closeAfterSave) profileOpen = false
      showToast(closeAfterSave ? 'Impostazioni salvate' : 'Avatar aggiornato')
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
    if (selected && !Array.isArray(selected)) await saveProfile(selected, false, false)
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

  async function saveContactQr() {
    if (!ownContactQr) return
    const path = await save({
      defaultPath: 'msnnext-contatto.png',
      filters: [{ name: 'Immagine PNG', extensions: ['png'] }],
    })
    if (!path) return
    await invoke('save_contact_qr', { path, dataUrl: ownContactQr })
      .then(() => showToast('QR salvato in alta qualità'))
      .catch((error) => showToast(String(error)))
  }

  async function shareDevicePairing() {
    if (!running) {
      showToast('Vai online per collegare un dispositivo')
      return
    }
    devicePairingMode = 'share'
    devicePairingOpen = true
    devicePairingLink = ''
    devicePairingQr = ''
    devicePairingBusy = true
    try {
      await invoke('node_request_device_link')
    } catch (error) {
      devicePairingBusy = false
      showToast(String(error))
    }
  }

  function joinDevicePairing() {
    if (!running) {
      showToast('Vai online per collegare questo dispositivo')
      return
    }
    devicePairingMode = 'join'
    devicePairingLink = ''
    devicePairingQr = ''
    devicePairingBusy = false
    devicePairingOpen = true
  }

  async function importDevicePairing() {
    if (!devicePairingLink.trim() || devicePairingBusy) return
    devicePairingBusy = true
    try {
      await invoke('node_import_device_link', { link: devicePairingLink.trim() })
    } catch (error) {
      devicePairingBusy = false
      showToast(String(error))
    }
  }

  async function scanDevicePairingQr() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: 'Immagini QR', extensions: ['png', 'jpg', 'jpeg', 'webp', 'gif'] }],
    })
    if (!selected || Array.isArray(selected)) return
    try {
      devicePairingLink = await invoke<string>('scan_contact_qr', { path: selected })
      await importDevicePairing()
    } catch (error) {
      showToast(String(error))
    }
  }

  async function copyDevicePairingLink() {
    if (!devicePairingLink) return
    await navigator.clipboard.writeText(devicePairingLink)
    showToast('Codice dispositivo copiato')
  }

  async function saveDevicePairingQr() {
    if (!devicePairingQr) return
    const path = await save({
      defaultPath: 'msnnext-dispositivo.png',
      filters: [{ name: 'Immagine PNG', extensions: ['png'] }],
    })
    if (!path) return
    await invoke('save_contact_qr', { path, dataUrl: devicePairingQr })
      .then(() => showToast('QR dispositivo salvato'))
      .catch((error) => showToast(String(error)))
  }

  async function prepareAccountBackupExport() {
    if (running) {
      showToast('Vai offline prima di creare un backup account')
      return
    }
    const path = await save({
      defaultPath: 'msnnext-account.msnnext-account',
      filters: [{ name: 'Backup account msnnext', extensions: ['msnnext-account'] }],
    })
    if (!path) return
    accountBackupMode = 'export'
    accountBackupPath = path
    accountBackupPassword = ''
    accountBackupOpen = true
  }

  async function prepareAccountBackupImport() {
    if (running) {
      showToast('Vai offline prima di ripristinare un account')
      return
    }
    const path = await open({
      multiple: false,
      directory: false,
      filters: [{ name: 'Backup account msnnext', extensions: ['msnnext-account'] }],
    })
    if (!path || Array.isArray(path)) return
    accountBackupMode = 'import'
    accountBackupPath = path
    accountBackupPassword = ''
    accountBackupOpen = true
  }

  async function submitAccountBackup() {
    if (accountBackupPassword.length < 12 || accountBackupBusy) return
    accountBackupBusy = true
    try {
      await invoke(accountBackupMode === 'export' ? 'account_backup_export' : 'account_backup_import', {
        password: accountBackupPassword,
        path: accountBackupPath,
      })
      accountBackupOpen = false
      accountBackupPassword = ''
      if (accountBackupMode === 'export') {
        showToast('Account, contatti e cronologia salvati')
        return
      }
      peerId = ''
      ownFingerprint = ''
      ownContactLink = ''
      ownContactQr = ''
      contacts = []
      conversations = {}
      selectedPeerId = ''
      selectedGroupId = ''
      profileOpen = false
      showToast('Account ripristinato')
      await startNode()
    } catch (error) {
      showToast(String(error))
    } finally {
      accountBackupBusy = false
    }
  }

  async function shakeWindow() {
    if (appWindow) {
      try {
        const origin = await appWindow.outerPosition()
        for (const offset of [-14, 14, -11, 11, -7, 7, 0]) {
          await appWindow.setPosition(new PhysicalPosition(origin.x + offset, origin.y))
          await new Promise((resolve) => window.setTimeout(resolve, 45))
        }
        return
      } catch {
        // La webview resta il fallback quando il window manager vieta lo spostamento.
      }
    }
    const frame = document.querySelector('.app-frame')
    frame?.classList.add('shake')
    window.setTimeout(() => frame?.classList.remove('shake'), 700)
  }

  function playNudgeSound() {
    if (!nudgeSound) return
    try {
      const context = new AudioContext()
      const oscillator = context.createOscillator()
      const gain = context.createGain()
      oscillator.type = 'square'
      oscillator.frequency.setValueAtTime(880, context.currentTime)
      oscillator.frequency.setValueAtTime(660, context.currentTime + 0.09)
      gain.gain.setValueAtTime(0.05, context.currentTime)
      gain.gain.exponentialRampToValueAtTime(0.001, context.currentTime + 0.18)
      oscillator.connect(gain).connect(context.destination)
      oscillator.start()
      oscillator.stop(context.currentTime + 0.18)
      oscillator.onended = () => void context.close()
    } catch {
      // Il trillo visivo resta disponibile se l'audio non può partire.
    }
  }

  function showToast(text: string) {
    toastText = text
    clearTimeout(toastTimer)
    toastTimer = setTimeout(() => toastText = '', 3200)
  }
</script>

<main class:details-open={detailsOpen} class:roster-open={rosterOpen} class="app-frame">
  <!-- svelte-ignore a11y_no_static_element_interactions -- native window drag has keyboard-accessible controls beside it -->
  <header class="app-titlebar" onmousedown={dragWindow} ondblclick={maximizeWindow}>
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
      {#if updateCandidate}
        <button
          class="titlebar-update"
          disabled={updateStatus === 'downloading' || updateStatus === 'installing'}
          title={`Installa msnnext ${updateCandidate.version}`}
          onclick={installUpdate}
        >
          <Download size={14} />
          <span>{updateStatus === 'downloading' ? `${updateProgress || '…'}%` : updateStatus === 'installing' ? 'Riavvio…' : `Aggiorna a ${updateCandidate.version}`}</span>
        </button>
      {/if}
      <span class:online={running} class="node-state"><i></i>{running ? 'Connesso' : 'Non connesso'}</span>
      <button class:online={running} class="power-button" aria-label={running ? 'Disconnetti' : 'Connetti'} title={running ? 'Disconnetti' : 'Connetti'} onclick={running ? stopNode : () => startNode(false)}>
        <Power size={16} />
      </button>
      <div class="window-controls">
        <button aria-label="Riduci a icona" title="Riduci a icona" onclick={() => void appWindow?.minimize()}><Minus size={15} /></button>
        <button aria-label="Ingrandisci o ripristina" title="Ingrandisci o ripristina" onclick={() => void appWindow?.toggleMaximize()}><Square size={12} /></button>
        <button class="window-close" aria-label="Chiudi" title="Chiudi" onclick={() => void appWindow?.close()}><X size={16} /></button>
      </div>
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
        <button aria-label="Apri impostazioni" title="Impostazioni" onclick={() => openSettings()}>
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
        {#if chatGroups.length}
          <div class="roster-section-label">Chat di gruppo</div>
          {#each chatGroups as group (group.id)}
            <button class:active={group.id === selectedGroupId} class="contact-row group-chat-row" oncontextmenu={(event) => showGroupMenu(event, group)} onclick={() => selectGroup(group.id)}>
              <span class="group-chat-avatar"><UsersRound size={18} /></span>
              <span class="contact-copy"><strong>{group.name}</strong><small>{group.members.length} partecipanti</small></span>
              <span class="roster-indicators">{#if isConversationMuted(groupConversationKey(group.id))}<BellOff class="muted-conversation" size={13} />{/if}{#if group.unread}<b class="unread">{group.unread}</b>{/if}</span>
            </button>
          {/each}
        {/if}
        {#if visibleContacts.length}
          <div class="roster-section-label">Contatti</div>
          {#each sortedContacts as contact (contact.peerId)}
            <button
              class:active={contact.peerId === selectedPeerId}
              class="contact-row"
              oncontextmenu={(event) => showContactMenu(event, contact)}
              onclick={() => selectContact(contact.peerId)}
            >
              <span class:offline={!contact.online} class="avatar-shell contact-avatar">
                <span>{contact.name.slice(0, 1).toUpperCase()}</span>
                <i class:online={contact.online}></i>
              </span>
              <span class="contact-copy"><strong>{contact.name}</strong><small>{contactSubtitle(contact)}</small></span>
              <span class="roster-indicators">{#if isConversationMuted(peerConversationKey(contact.peerId))}<BellOff class="muted-conversation" size={13} />{/if}{#if contact.unread}<b class="unread">{contact.unread}</b>{/if}</span>
            </button>
          {/each}
        {:else if contacts.length && !chatGroups.length}
          <div class="empty-contacts compact">
            <strong>Nessun risultato</strong>
            <p>Prova a cercare con un altro nome.</p>
          </div>
        {:else if !chatGroups.length}
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
        <button onclick={openGroupCreation}><UsersRound size={15} /> Nuovo gruppo</button>
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
            {#if activeGroup}<UsersRound size={20} />{:else}<span>{activeContact?.name.slice(0, 1).toUpperCase() || '?'}</span>{/if}
            {#if activeContact}<i class:online={activeContact.online}></i>{/if}
          </div>
          <span>
            <strong>{activeGroup?.name || activeContact?.name || 'msnnext'}</strong>
            <small>
              {activeGroup ? `${groupOnline} partecipanti collegati · canali protetti` : ready ? 'Disponibile · conversazione protetta' : activeContact?.online ? 'Sto preparando la conversazione…' : activeContact ? 'Non in linea' : 'Scegli una conversazione dalla lista'}
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
          {#if !activeContact && !activeGroup}
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
                {#if activeGroup}<UsersRound size={30} />{:else}<span>{activeContact?.name.slice(0, 1).toUpperCase()}</span><i class:online={activeContact?.online}></i>{/if}
              </div>
              <h2>{activeGroup?.name || activeContact?.name}</h2>
              <p>{activeGroup ? (ready ? 'Scrivi il primo messaggio al gruppo.' : 'La chat sarà disponibile quando almeno un partecipante sarà online.') : ready ? 'È online. Scrivi il primo messaggio o manda un trillo.' : 'Quando tornerà online potrete riprendere a parlare.'}</p>
            </div>
          {:else}
            <div class="session-start"><span>Inizio della conversazione</span></div>
            {#each messages as message (message.id)}
              {#if message.kind === 'nudge'}
                <div class="nudge-message">
                  <span><Zap size={18} /></span>
                  <p><strong>{message.mine ? 'Hai inviato un trillo!' : `${activeContact?.name || 'Un contatto'} ti ha inviato un trillo!`}</strong><small>La finestra ha fatto un piccolo salto.</small></p>
                  <time>{message.time}</time>
                </div>
              {:else}
                <article class:mine={message.mine} class:file-message={message.kind === 'file'} class="message-line">
                  <header>
                  <strong>{senderName(message)}</strong>
                    <time>{message.time}</time>
                  </header>
                  {#if message.kind === 'file'}
                    <button class="file-line" disabled={!message.attachmentId} onclick={() => openAttachment(message)}>
                      {#if message.attachmentDataUrl}
                        <img src={message.attachmentDataUrl} alt={message.body} />
                      {:else}<Paperclip size={17} />{/if}
                      <span><b>{message.attachmentMime?.startsWith('image/') ? (message.mine ? 'Immagine inviata' : 'Immagine ricevuta') : (message.mine ? 'File inviato' : 'File ricevuto')}</b><small>{message.body}</small></span>
                      {#if message.attachmentId}<ExternalLink size={14} />{/if}
                    </button>
                  {:else}
                    <p>
                      {#each messageParts(message, [...customEmoticons, ...offeredEmoticons]) as part}
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
                {#if activeGroup}<UsersRound size={25} />{:else}<span>{activeContact?.name.slice(0, 1).toUpperCase() || displayName.slice(0, 1).toUpperCase()}</span><i class:online={activeContact?.online || running}></i>{/if}
              </div>
              <strong>{activeGroup?.name || activeContact?.name || displayName}</strong>
              <small>{activeGroup ? `${activeGroup.members.length} partecipanti` : activeContact ? (activeContact.online ? 'Disponibile' : 'Non in linea') : (running ? 'Online' : 'Non in linea')}</small>
            </div>
            <section class="detail-section">
              <h3>Sicurezza</h3>
              <div class="detail-row">
                <span><ShieldCheck size={18} /></span>
                <p><strong>Conversazione protetta</strong><small>{ready ? 'Canale cifrato; confronta il codice identità' : 'Disponibile quando il contatto è online'}</small></p>
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
              <h3>{activeContact ? 'Codice identità del contatto' : 'La tua identità'}</h3>
              <code>{activeContact?.fingerprint || ownFingerprint || 'Disponibile dopo l’avvio'}</code>
              <small>Confrontalo a voce o tramite il QR prima di considerare verificata l’identità.</small>
              <button disabled={!running || linkRequested} onclick={openContacts}><QrCode size={15} /> Mostra QR e link</button>
            </section>
            {#if activeContact}
              <section class="detail-section contact-management">
                <h3>Gestione contatto</h3>
                <div class="contact-name-editor">
                  <label for="contact-display-name">Nome visualizzato</label>
                  <div class="contact-name-row"><input id="contact-display-name" bind:value={contactName} maxlength="64" placeholder={activeContact.name} /><button onclick={renameContact}><Pencil size={14} /> Salva</button></div>
                </div>
                <div class="contact-danger-zone">
                  <button onclick={clearConversation}><Trash2 size={14} /><span><strong>Cancella cronologia</strong><small>Il contatto rimane nella lista</small></span></button>
                  <button class="danger-button" onclick={deleteContact}><Trash2 size={14} /><span><strong>Rimuovi contatto</strong><small>Elimina anche la conversazione</small></span></button>
                </div>
              </section>
            {/if}
            {#if activeGroup}
              <section class="detail-section group-management">
                <h3>Partecipanti</h3>
                <ul>
                  {#each activeGroup.members as member}
                    {@const ban = memberBan(activeGroup, member)}
                    <li class="group-member-row">
                      <span class="group-member-copy">
                        <strong>{memberName(member)}</strong>
                        <small>
                          <b>{memberRole(activeGroup, member)}</b>
                          {#if activeGroup.silenced.includes(member)}<i>Silenziato</i>{/if}
                          {#if ban}<i class="ban-status">{banLabel(ban)}</i>{/if}
                        </small>
                      </span>
                      {#if canModerateMember(activeGroup, member)}
                        <select aria-label={`Gestisci ${memberName(member)}`} value="" onchange={(event) => moderateGroup(member, event.currentTarget.value)}>
                          <option value="">Gestisci…</option>
                          {#if peerId === activeGroup.ownerPeerId}
                            <option value={activeGroup.admins.includes(member) ? 'member' : 'admin'}>{activeGroup.admins.includes(member) ? 'Rendi membro' : 'Rendi amministratore'}</option>
                          {/if}
                          {#if !activeGroup.admins.includes(member)}
                            {#if ban}
                              <option value="unban">Rimuovi ban</option>
                            {:else}
                              <option value={activeGroup.silenced.includes(member) ? 'unsilence' : 'silence'}>{activeGroup.silenced.includes(member) ? 'Rimuovi silence' : 'Silenzia'}</option>
                              <option value="tempBan:3600000">Ban per 1 ora</option>
                              <option value="tempBan:86400000">Ban per 24 ore</option>
                              <option value="tempBan:604800000">Ban per 7 giorni</option>
                              <option value="permaBan">Ban permanente</option>
                            {/if}
                          {/if}
                        </select>
                      {/if}
                    </li>
                  {/each}
                </ul>
                <button onclick={clearGroupConversation}><Trash2 size={14} /> Elimina cronologia</button>
                <button class="danger-button" onclick={deleteChatGroup}><Trash2 size={14} /> Rimuovi chat dal dispositivo</button>
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
          <button class:active={emojiOpen} disabled={!canSend} onclick={() => emojiOpen = !emojiOpen}><Smile size={18} /><span>Emoticon</span></button>
          <button class="nudge-tool" disabled={!ready || !!activeGroup} onclick={sendNudge}><Zap size={18} /><span>Trillo</span></button>
          <button disabled={!canSend || fileSending} onclick={chooseFile}><Paperclip size={18} /><span>{fileSending ? 'Invio…' : 'Invia file'}</span></button>
          {#if fileSending}<button class="danger-item" onclick={cancelFileTransfers}><X size={18} /><span>Annulla</span></button>{/if}
        </div>
        {#if fileSending && transferFilename}
          <div class="transfer-progress"><span>{transferFilename}</span><progress max="100" value={transferProgress}></progress><strong>{transferProgress}%</strong></div>
        {/if}
        <form class="composer" onsubmit={(event) => { event.preventDefault(); void sendMessage() }}>
          <div
            bind:this={messageEditor}
            class="message-editor"
            class:disabled={!canSend}
            contenteditable={canSend}
            role="textbox"
            tabindex={canSend ? 0 : -1}
            aria-label="Messaggio"
            aria-multiline="true"
            aria-disabled={!canSend}
            data-placeholder={!groupCanSend ? 'Non puoi scrivere: sei silenziato o bannato.' : ready ? `Scrivi a ${activeGroup?.name || activeContact?.name}…` : activeGroup ? 'Nessun partecipante è disponibile.' : activeContact ? 'Il contatto non è disponibile.' : 'Scegli una conversazione per scrivere.'}
            oninput={syncDraft}
            onpaste={pasteDraft}
            ondrop={(event) => event.preventDefault()}
            onkeydown={(event) => {
              if (event.key === 'Enter' && !event.isComposing) {
                event.preventDefault()
                if (event.shiftKey) {
                  insertAtDraftCaret('\n')
                  syncDraft()
                } else void sendMessage()
              }
            }}
          ></div>
          <button type="submit" class="send-button" disabled={!canSend || !messageText.trim()}><Send size={17} /> Invia</button>
        </form>
        <small class="composer-hint">Invio per spedire · Maiusc+Invio per andare a capo</small>
      </footer>
      {#if fileDropActive && canSend}
        <div class="file-drop-overlay"><Paperclip size={28} /><strong>Rilascia per inviare</strong><small>File e immagini, massimo 5 GB ciascuno</small></div>
      {/if}
    </section>
  </div>
</main>

{#if contextPeerId || contextGroupId}
  {@const contextConversation = contextGroupId ? groupConversationKey(contextGroupId) : peerConversationKey(contextPeerId)}
  <button class="context-scrim" aria-label="Chiudi menu" onclick={closeContextMenu}></button>
  <div class="contact-context-menu" style={`left:${contextX}px;top:${contextY}px`} role="menu">
    <button onclick={() => { contextGroupId ? selectGroup(contextGroupId) : selectContact(contextPeerId); closeContextMenu() }}>Apri conversazione</button>
    {#if contextPeerId}<button onclick={() => manageContact(contextPeerId)}>Rinomina e gestisci</button>{/if}
    <div class="context-separator"></div>
    <small>Notifiche</small>
    {#if isConversationMuted(contextConversation)}
      <button onclick={() => unmuteConversation(contextConversation)}>Riattiva notifiche</button>
    {:else}
      <button onclick={() => muteConversation(contextConversation, 60 * 60 * 1000)}>Silenzia per 1 ora</button>
      <button onclick={() => muteConversation(contextConversation, 8 * 60 * 60 * 1000)}>Silenzia per 8 ore</button>
      <button onclick={() => muteConversation(contextConversation, null)}>Silenzia sempre</button>
    {/if}
    {#if contextPeerId}
    <div class="context-separator"></div>
    <button class="danger-item" onclick={() => { selectContact(contextPeerId); closeContextMenu(); void deleteContact() }}>Elimina contatto</button>
    {/if}
  </div>
{/if}

{#if mediaPreview}
  <div class="modal-backdrop image-viewer" role="dialog" aria-modal="true" aria-label="Anteprima allegato" tabindex="-1">
    <button aria-label="Chiudi anteprima" onclick={() => mediaPreview = ''}><X size={20} /></button>
    {#if mediaPreview.startsWith('data:video/')}
      <!-- svelte-ignore a11y_media_has_caption i file ricevuti non includono una traccia sottotitoli separata -->
      <video src={mediaPreview} controls autoplay aria-label="Video ricevuto"></video>
    {:else}
      <img src={mediaPreview} alt="Immagine ricevuta" />
    {/if}
  </div>
{/if}

{#if incomingAttachmentOffers[0]}
  {@const offer = incomingAttachmentOffers[0]}
  <div class="modal-backdrop">
    <div class="modal" role="dialog" aria-modal="true" aria-labelledby="attachment-offer-title">
      <p class="step-label">File in arrivo</p>
      <h2 id="attachment-offer-title">Vuoi ricevere {offer.filename}?</h2>
      <p>{contacts.find((contact) => contact.peerId === offer.peerId)?.name || 'Un contatto'} vuole inviarti un file da {formatBytes(offer.size)}.</p>
      <div class="modal-actions">
        <button class="secondary-button" onclick={() => answerAttachmentOffer(offer, false)}>Rifiuta</button>
        <button class="primary-button" onclick={() => answerAttachmentOffer(offer, true)}>Accetta</button>
      </div>
    </div>
  </div>
{/if}

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
          <label>Relay personalizzato <small>facoltativo</small><input bind:value={relayAddress} maxlength="512" placeholder="Usa il mininodo MSN Next" /></label>
        </details>
        <button class="primary-button wide" disabled={starting || !displayName.trim()} onclick={() => startNode()}>
          {starting ? 'Connessione in corso…' : 'Vai online'}
        </button>
        <button class="secondary-button wide" onclick={prepareAccountBackupImport}><Upload size={14} /> Ripristina account esistente</button>
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
          <div class="contact-share-actions">
            <button class="copy-link" onclick={copyOwnLink}><Copy size={15} /> Copia il link</button>
            <button class="copy-link" onclick={saveContactQr}><QrCode size={15} /> Salva QR grande</button>
          </div>
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

{#if groupCreateOpen}
  <div class="modal-backdrop">
    <div class="modal group-create-modal" role="dialog" aria-modal="true" aria-labelledby="group-create-title">
      <button class="modal-close" aria-label="Chiudi" onclick={() => groupCreateOpen = false}><X size={18} /></button>
      <div class="modal-heading">
        <span><UsersRound size={23} /></span>
        <div><p class="step-label">Conversazione condivisa</p><h2 id="group-create-title">Nuova chat di gruppo</h2></div>
      </div>
      <label>Nome del gruppo<input bind:value={groupName} maxlength="64" placeholder="Per esempio: Amici" /></label>
      <fieldset class="group-member-picker">
        <legend>Scegli almeno due persone</legend>
        {#each contacts as contact (contact.peerId)}
          <label>
            <input type="checkbox" checked={groupMemberIds.includes(contact.peerId)} onchange={() => toggleGroupMember(contact.peerId)} />
            <span><strong>{contact.name}</strong><small>{contact.secure ? 'Online e protetto' : contact.online ? 'Connessione in preparazione' : 'Non in linea'}</small></span>
          </label>
        {/each}
      </fieldset>
      <button class="primary-button wide" disabled={!groupName.trim() || groupMemberIds.length < 2 || pendingGroupCreation} onclick={createChatGroup}>{pendingGroupCreation ? 'Creo il gruppo…' : `Crea gruppo con ${groupMemberIds.length + 1} partecipanti`}</button>
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
    <div class="modal settings-modal" role="dialog" aria-modal="true" aria-labelledby="settings-title">
      <button class="modal-close" aria-label="Chiudi" onclick={() => profileOpen = false}><X size={18} /></button>
      <header class="settings-header">
        <div class="settings-avatar avatar-shell">
          {#if avatarDataUrl}<img src={avatarDataUrl} alt="Avatar personale" />{:else}<span>{displayName.slice(0, 1).toUpperCase()}</span>{/if}
        </div>
        <div>
          <p class="step-label">msnnext {appVersion}</p>
          <h2 id="settings-title">Impostazioni</h2>
          <small>{displayName}</small>
        </div>
      </header>

      <div class="settings-shell">
        <nav class="settings-navigation" aria-label="Sezioni impostazioni">
          <button class:active={settingsSection === 'profile'} aria-current={settingsSection === 'profile' ? 'page' : undefined} onclick={() => settingsSection = 'profile'}><UserRound size={17} /><span>Profilo</span></button>
          <button class:active={settingsSection === 'appearance'} aria-current={settingsSection === 'appearance' ? 'page' : undefined} onclick={() => settingsSection = 'appearance'}><Palette size={17} /><span>Aspetto</span></button>
          <button class:active={settingsSection === 'devices'} aria-current={settingsSection === 'devices' ? 'page' : undefined} onclick={() => settingsSection = 'devices'}><Monitor size={17} /><span>Dispositivi</span></button>
          <button class:active={settingsSection === 'data'} aria-current={settingsSection === 'data' ? 'page' : undefined} onclick={() => settingsSection = 'data'}><Database size={17} /><span>Dati</span></button>
          <button class:active={settingsSection === 'updates'} aria-current={settingsSection === 'updates' ? 'page' : undefined} onclick={() => settingsSection = 'updates'}>
            <RefreshCw size={17} /><span>Aggiornamenti</span>{#if updateCandidate}<i aria-label="Aggiornamento disponibile"></i>{/if}
          </button>
          <button class:active={settingsSection === 'network'} aria-current={settingsSection === 'network' ? 'page' : undefined} onclick={() => settingsSection = 'network'}><Radio size={17} /><span>Rete e sicurezza</span></button>
        </nav>

        <div class="settings-content">
          {#if settingsSection === 'profile'}
            <section class="settings-panel" aria-labelledby="profile-panel-title">
              <header class="settings-panel-heading">
                <h3 id="profile-panel-title">Profilo personale</h3>
                <p>Le persone collegate vedono questo nome e questa immagine.</p>
              </header>
              <div class="profile-settings-editor">
                <div class="profile-editor-avatar avatar-shell">
                  {#if avatarDataUrl}<img src={avatarDataUrl} alt="Avatar personale" />{:else}<span>{displayName.slice(0, 1).toUpperCase()}</span>{/if}
                </div>
                <div class="profile-avatar-copy">
                  <strong>Immagine del profilo</strong>
                  <small>PNG, JPEG o WebP</small>
                  <div class="profile-avatar-actions">
                    <button class="secondary-button" onclick={chooseAvatar}>Scegli</button>
                    {#if avatarDataUrl}<button class="secondary-button" onclick={() => saveProfile(null, true, false)}>Rimuovi</button>{/if}
                  </div>
                </div>
              </div>
              <label class="settings-field">Nome visualizzato<input bind:value={displayName} maxlength="64" /></label>
            </section>
          {:else if settingsSection === 'appearance'}
            <section class="settings-panel" aria-labelledby="appearance-panel-title">
              <header class="settings-panel-heading">
                <h3 id="appearance-panel-title">Aspetto e comportamento</h3>
                <p>Personalizza leggibilità, anteprime e avvisi.</p>
              </header>
              <div class="settings-theme-control" role="group" aria-label="Tema dell'app">
                <button class:active={theme === 'light'} onclick={() => setTheme('light')}><Sun size={16} /> Chiaro</button>
                <button class:active={theme === 'dark'} onclick={() => setTheme('dark')}><Moon size={16} /> Scuro</button>
                <button class:active={theme === 'system'} onclick={() => setTheme('system')}><Monitor size={16} /> Sistema</button>
              </div>
              <div class="settings-list">
                <label class="settings-row"><span><strong>Dimensione testo</strong><small>L'anteprima cambia immediatamente</small></span><select bind:value={fontScale} aria-label="Dimensione testo"><option value={100}>Originale</option><option value={115}>Comoda</option><option value={125}>Grande</option><option value={140}>Molto grande</option></select></label>
                <label class="settings-row"><span><strong>Immagini inviate</strong><small>Mostrale appena premi invio</small></span><input type="checkbox" bind:checked={previewSentImages} /></label>
                <label class="settings-row"><span><strong>Immagini ricevute</strong><small>Mostrale senza doverle aprire</small></span><input type="checkbox" bind:checked={previewReceivedImages} /></label>
                <label class="settings-row"><span><strong>Suono del trillo</strong><small>Riproduci un avviso quando ricevi un trillo</small></span><input type="checkbox" bind:checked={nudgeSound} /></label>
              </div>
            </section>
          {:else if settingsSection === 'devices'}
            <section class="settings-panel" aria-labelledby="linked-devices-title">
              <header class="settings-panel-heading">
                <h3 id="linked-devices-title">Dispositivi collegati</h3>
                <p>Contatti e cronologia passano direttamente tra i client online.</p>
              </header>
              {#if linkedDevices.length}
                <div class="linked-device-list settings-list">
                  {#each linkedDevices as device (device.peerId)}
                    <div class="linked-device-row settings-row">
                      <span class:online={device.online} class="device-status-dot"></span>
                      <span><strong>{device.name}</strong><small>{deviceStatus(device)}</small></span>
                    </div>
                  {/each}
                </div>
              {:else}
                <div class="settings-empty"><Monitor size={22} /><strong>Solo questo dispositivo</strong><small>Collegane un altro per sincronizzare i dati mentre entrambi sono online.</small></div>
              {/if}
              <div class="settings-action-row">
                <button class="secondary-button" disabled={!running} onclick={shareDevicePairing}><QrCode size={14} /> Mostra codice</button>
                <button class="secondary-button" disabled={!running} onclick={joinDevicePairing}><Link2 size={14} /> Usa codice</button>
              </div>
            </section>
          {:else if settingsSection === 'data'}
            <section class="settings-panel" aria-labelledby="data-panel-title">
              <header class="settings-panel-heading">
                <h3 id="data-panel-title">Dati e contenuti</h3>
                <p>Gestisci backup di emergenza ed emoticon personali.</p>
              </header>
              <div class="settings-subsection">
                <div class="settings-subsection-heading"><span><strong>Backup cifrato</strong><small>Include account, contatti, messaggi e gruppi, ma non gli allegati.</small></span><ShieldCheck size={18} /></div>
                <div class="settings-action-row">
                  <button class="secondary-button" disabled={running} onclick={prepareAccountBackupExport}><Download size={14} /> Esporta</button>
                  <button class="secondary-button" disabled={running} onclick={prepareAccountBackupImport}><Upload size={14} /> Importa</button>
                </div>
                {#if running}<p class="settings-note">Vai offline per esportare o ripristinare un backup.</p>{/if}
              </div>
              <div class="settings-subsection settings-emoticons-flat">
                <div class="settings-subsection-heading">
                  <span><strong>Emoticon personali</strong><small>Disponibili anche quando i contatti sono offline.</small></span>
                  <button class="secondary-button" onclick={chooseEmoticonFile}><Plus size={14} /> Crea</button>
                </div>
                {#if customEmoticons.length}
                  <div class="emoji-grid custom-emoji-grid">
                    {#each customEmoticons as item (item.assetId)}
                      <button aria-label={`Modifica ${item.name}`} title="Modifica o elimina" onclick={() => openSaveEmoticon(item)}><img src={item.dataUrl} alt="" /><small>{item.trigger}</small></button>
                    {/each}
                  </div>
                {:else}
                  <div class="settings-empty compact"><Smile size={20} /><strong>Nessuna emoticon personale</strong></div>
                {/if}
                {#if offeredEmoticons.length}
                  <small class="emoji-section-label">Ricevute da salvare</small>
                  <div class="received-emoji-list">
                    {#each offeredEmoticons as item (item.assetId)}
                      <div><img src={item.dataUrl} alt={item.name} /><span><strong>{item.name}</strong><small>{item.trigger}</small></span><button onclick={() => openSaveEmoticon(item)}>Salva</button></div>
                    {/each}
                  </div>
                {/if}
              </div>
            </section>
          {:else if settingsSection === 'updates'}
            <section class="settings-panel" aria-labelledby="updates-panel-title">
              <header class="settings-panel-heading">
                <h3 id="updates-panel-title">Aggiornamenti</h3>
                <p>msnnext controlla automaticamente le nuove versioni ogni cinque ore.</p>
              </header>
              <div class:available={!!updateCandidate} class:error={updateStatus === 'error'} class="update-status-panel">
                <span class:spinning={updateStatus === 'checking'}>
                  {#if updateCandidate}<Download size={22} />{:else if updateStatus === 'current'}<CheckCircle2 size={22} />{:else}<RefreshCw size={22} />{/if}
                </span>
                <div><strong>{updateCandidate ? `msnnext ${updateCandidate.version} disponibile` : `msnnext ${appVersion}`}</strong><small>{updateMessage || 'Il controllo automatico è attivo.'}</small></div>
              </div>
              {#if updateStatus === 'downloading' || updateStatus === 'installing'}
                <div class="update-progress" aria-label={`Aggiornamento al ${updateProgress}%`}><i style={`width: ${updateProgress}%`}></i></div>
              {/if}
              <div class="update-meta"><span>Versione installata<strong>{appVersion}</strong></span><span>Ultimo controllo<strong>{lastUpdateCheckLabel()}</strong></span></div>
              <div class="settings-action-row update-actions">
                <button class="secondary-button" disabled={updateStatus === 'checking' || updateStatus === 'downloading' || updateStatus === 'installing'} onclick={() => checkForUpdates(true)}><RefreshCw size={14} /> Controlla ora</button>
                {#if updateCandidate}<button class="primary-button" disabled={updateStatus === 'downloading' || updateStatus === 'installing'} onclick={installUpdate}><Download size={14} /> {updateStatus === 'downloading' ? `Download ${updateProgress || '…'}%` : 'Scarica e installa'}</button>{/if}
              </div>
            </section>
          {:else}
            <section class="settings-panel" aria-labelledby="network-panel-title">
              <header class="settings-panel-heading">
                <h3 id="network-panel-title">Rete e sicurezza</h3>
                <p>Il mininodo aiuta i dispositivi a trovarsi; i dati restano cifrati tra client.</p>
              </header>
              <label class="settings-field">Relay personalizzato<input bind:value={relayAddress} maxlength="512" placeholder="Usa il mininodo msnnext" /><small>Lascia vuoto per usare automaticamente il mininodo pubblico. Riconnettiti dopo averlo cambiato.</small></label>
              <button class="security-settings-row" onclick={() => securityIntroOpen = true}><ShieldCheck size={19} /><span><strong>Protezione dei tuoi dati</strong><small>Cifratura, identità locale e limiti del modello di sicurezza</small></span><ExternalLink size={15} /></button>
            </section>
          {/if}
        </div>
      </div>

      <footer class="settings-footer">
        <small>Le modifiche al tema e al testo sono visibili subito.</small>
        <div><button class="secondary-button" onclick={() => profileOpen = false}>Annulla</button><button class="primary-button" disabled={!displayName.trim()} onclick={() => saveProfile()}>Salva modifiche</button></div>
      </footer>
    </div>
  </div>
{/if}

{#if devicePairingOpen}
  <div class="modal-backdrop">
    <div class="modal device-pairing-modal" role="dialog" aria-modal="true" aria-labelledby="device-pairing-title">
      <button type="button" class="modal-close" aria-label="Chiudi" onclick={() => devicePairingOpen = false}><X size={18} /></button>
      <div class="modal-heading">
        <span>{#if devicePairingMode === 'share'}<QrCode size={22} />{:else}<Link2 size={22} />{/if}</span>
        <div><p class="step-label">Sincronizzazione privata</p><h2 id="device-pairing-title">{devicePairingMode === 'share' ? 'Collega un altro dispositivo' : 'Collega questo dispositivo'}</h2></div>
      </div>
      {#if devicePairingMode === 'share'}
        <p>Apri msnnext sull'altro dispositivo e usa questo codice entro dieci minuti. Entrambi devono restare online.</p>
        {#if devicePairingQr}
          <button class="device-pairing-qr" aria-label="Salva QR dispositivo" title="Salva QR" onclick={saveDevicePairingQr}><img src={devicePairingQr} alt="QR per collegare il dispositivo" /></button>
          <div class="device-pairing-actions">
            <button class="secondary-button" onclick={copyDevicePairingLink}><Copy size={14} /> Copia codice</button>
            <button class="secondary-button" onclick={saveDevicePairingQr}><Download size={14} /> Salva QR</button>
          </div>
          <small>Scade alle {new Date(devicePairingExpiresAt).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}.</small>
        {:else}
          <div class="device-pairing-loading">Preparazione del collegamento…</div>
        {/if}
      {:else}
        <p>Incolla il codice mostrato dal dispositivo già collegato, oppure apri un'immagine del QR.</p>
        <label>Codice dispositivo<textarea bind:value={devicePairingLink} rows="4" spellcheck="false" placeholder="msnnext://device/…"></textarea></label>
        <div class="device-pairing-actions">
          <button class="secondary-button" disabled={devicePairingBusy} onclick={scanDevicePairingQr}><QrCode size={14} /> Apri QR</button>
          <button class="primary-button" disabled={!devicePairingLink.trim() || devicePairingBusy} onclick={importDevicePairing}>{devicePairingBusy ? 'Collegamento…' : 'Collega'}</button>
        </div>
      {/if}
    </div>
  </div>
{/if}

{#if accountBackupOpen}
  <div class="modal-backdrop">
    <div class="modal account-backup-modal" role="dialog" aria-modal="true" aria-labelledby="account-backup-title">
      <button type="button" class="modal-close" aria-label="Chiudi" onclick={() => accountBackupOpen = false}><X size={18} /></button>
      <form onsubmit={(event) => { event.preventDefault(); void submitAccountBackup() }}>
        <div class="modal-heading">
          <span>{#if accountBackupMode === 'export'}<Download size={22} />{:else}<Upload size={22} />{/if}</span>
          <div><p class="step-label">Account msnnext</p><h2 id="account-backup-title">{accountBackupMode === 'export' ? 'Proteggi il backup' : 'Ripristina il tuo account'}</h2></div>
        </div>
        <p>{accountBackupMode === 'export' ? 'Identità, contatti e cronologia saranno cifrati. Scegli una password da usare sul nuovo PC.' : 'Inserisci la password per ripristinare identità, contatti e cronologia.'}</p>
        <label>Password<input type="password" bind:value={accountBackupPassword} minlength="12" autocomplete={accountBackupMode === 'export' ? 'new-password' : 'current-password'} /></label>
        <small>Almeno 12 caratteri. La password non può essere recuperata.</small>
        <button class="primary-button wide" disabled={accountBackupPassword.length < 12 || accountBackupBusy}>
          {accountBackupBusy ? 'Attendi…' : accountBackupMode === 'export' ? 'Crea backup cifrato' : 'Ripristina account'}
        </button>
      </form>
    </div>
  </div>
{/if}

{#if securityIntroOpen}
  <div class="modal-backdrop security-intro-backdrop">
    <div class="modal security-intro-modal" role="dialog" aria-modal="true" aria-labelledby="security-intro-title">
      <div class="modal-heading">
        <span><ShieldCheck size={24} /></span>
        <div><p class="step-label">Prima di iniziare</p><h2 id="security-intro-title">Quanto è sicuro msnnext?</h2></div>
      </div>
      <p class="security-intro-lead">È progettato per proteggere conversazioni e file senza affidarli a un server centrale.</p>
      <ul class="security-intro-list">
        <li><LockKeyhole size={18} /><span><strong>Cifratura tra dispositivi</strong><small>Messaggi, trilli e file sono cifrati con XChaCha20-Poly1305; eventuali nodi di inoltro non possono leggerne il contenuto.</small></span></li>
        <li><Sparkles size={18} /><span><strong>Protezione ibrida post-quantum</strong><small>Lo scambio delle chiavi combina X25519 e ML-KEM-768: se una delle due protezioni resta sicura, la sessione resta protetta.</small></span></li>
        <li><ShieldCheck size={18} /><span><strong>Dati locali protetti</strong><small>Cronologia e allegati ricevuti sono cifrati sul dispositivo; la chiave d’identità è custodita dal sistema operativo.</small></span></li>
      </ul>
      <div class="security-caveat"><Info size={16} /><p><strong>Nessun software è sicuro al 100%.</strong> L’indirizzo IP e gli orari delle connessioni possono essere visibili ai partecipanti; questa versione è ancora in sviluppo e non ha ricevuto un audit indipendente.</p></div>
      <button class="primary-button wide" onclick={closeSecurityIntro}>Ho capito, iniziamo</button>
    </div>
  </div>
{/if}

{#if toastText}
  <div class="toast" role="status"><MessageCircleMore size={16} />{toastText}</div>
{/if}
