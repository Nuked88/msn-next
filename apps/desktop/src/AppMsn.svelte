<script lang="ts">
  import { invoke, isTauri } from '@tauri-apps/api/core'
  import { getVersion } from '@tauri-apps/api/app'
  import { listen, type UnlistenFn } from '@tauri-apps/api/event'
  import { getCurrentWebview } from '@tauri-apps/api/webview'
  import { getCurrentWindow, UserAttentionType } from '@tauri-apps/api/window'
  import { Image } from '@tauri-apps/api/image'
  import { PhysicalPosition } from '@tauri-apps/api/dpi'
  import { open, save } from '@tauri-apps/plugin-dialog'
  import { isPermissionGranted, requestPermission, sendNotification } from '@tauri-apps/plugin-notification'
  import { relaunch } from '@tauri-apps/plugin-process'
  import { check, type Update } from '@tauri-apps/plugin-updater'
  import QRCode from 'qrcode'
  import { sounds } from './lib/sounds'
  import { t, locale, available as availableLocales } from './lib/i18n'
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
    QrCode,
    Radio,
    RefreshCw,
    Send,
    Settings,
    Share2,
    ShieldAlert,
    ShieldCheck,
    ShieldOff,
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
    eventId?: string
    relayed?: boolean
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
    eventId?: string
    deleted?: boolean
    timestampMs?: number
    relayed?: boolean
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
    | { type: 'contactRequest'; peerId: string; name: string }
    | { type: 'messageDeleted'; peerId: string; eventId: string }
    | { type: 'contactStatus'; peerId: string; status: string }
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
    startMinimized: boolean
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
    { glyph: '😐', shortcut: ':|', label: 'Neutro' },
    { glyph: '😳', shortcut: ':$', label: 'Imbarazzato' },
    { glyph: '😘', shortcut: ':*', label: 'Bacio' },
    { glyph: '😠', shortcut: ':@', label: 'Arrabbiato' },
    { glyph: '😇', shortcut: 'O:)', label: 'Angelo' },
    { glyph: '😈', shortcut: '}:)', label: 'Diavoletto' },
    { glyph: '😴', shortcut: '|-)', label: 'Assonnato' },
    { glyph: '👍', shortcut: '(y)', label: 'Pollice su' },
    { glyph: '👎', shortcut: '(n)', label: 'Pollice giù' },
    { glyph: '🌹', shortcut: '@}-', label: 'Rosa' },
    { glyph: '🎉', shortcut: '\\o/', label: 'Festa' },
  ]

  const appWindow = isTauri() ? getCurrentWindow() : null

  // Angoli finestra arrotondati solo su desktop (su mobile è fullscreen). Va
  // impostato subito (non in onMount) così il contenuto è già ritagliato al
  // primo paint, altrimenti il body opaco squadrato buca l'arrotondamento.
  if (typeof document !== 'undefined' && typeof navigator !== 'undefined'
      && !/Android|iPhone|iPad|iPod/i.test(navigator.userAgent)) {
    document.documentElement.classList.add('desktop')
  }

  const securityIntroKey = 'msnnext-security-intro-v1'
  const notificationMutesKey = 'msnnext-notification-mutes-v1'
  const lastUpdateCheckKey = 'msnnext-update-last-check-v1'
  const effectsSoundsKey = 'msnnext-effects-sounds-v1'
  const updateCheckIntervalMs = 5 * 60 * 60 * 1000

  // Master toggle for message/sign-in effect sounds (nudge keeps its own setting).
  let effectsSounds = typeof localStorage === 'undefined'
    ? true
    : localStorage.getItem(effectsSoundsKey) !== '0'
  $: if (typeof localStorage !== 'undefined') localStorage.setItem(effectsSoundsKey, effectsSounds ? '1' : '0')

  // Notifiche di sistema (desktop + Android), disabilitabili.
  const notificationsEnabledKey = 'msnnext-notifications-v1'
  let notificationsEnabled = typeof localStorage === 'undefined'
    ? true
    : localStorage.getItem(notificationsEnabledKey) !== '0'
  $: if (typeof localStorage !== 'undefined') localStorage.setItem(notificationsEnabledKey, notificationsEnabled ? '1' : '0')
  let notifPermissionGranted = false

  async function ensureNotifPermission(): Promise<boolean> {
    if (!isTauri()) return false
    if (notifPermissionGranted) return true
    try {
      notifPermissionGranted = await isPermissionGranted()
      if (!notifPermissionGranted) notifPermissionGranted = (await requestPermission()) === 'granted'
    } catch { notifPermissionGranted = false }
    return notifPermissionGranted
  }

  async function osNotify(title: string, body: string) {
    if (!notificationsEnabled) return
    if (!(await ensureNotifPermission())) return
    try { sendNotification({ title, body }) } catch (error) { console.warn('notifica non inviata', error) }
  }

  // Auto-accept media: estensioni consentite (stringa comma/spazio separata).
  const autoAcceptExtKey = 'msnnext-auto-accept-ext-v1'
  const autoAcceptAllKey = 'msnnext-auto-accept-all-v1'
  let autoAcceptExtensions = typeof localStorage === 'undefined'
    ? ''
    : (localStorage.getItem(autoAcceptExtKey) || '')
  let autoAcceptAll = typeof localStorage !== 'undefined' && localStorage.getItem(autoAcceptAllKey) === '1'
  function parseExtensions(value: string): string[] {
    return value.split(/[\s,]+/).map((ext) => ext.trim().replace(/^\./, '').toLowerCase()).filter(Boolean)
  }
  async function applyAutoAccept() {
    if (!isTauri()) return
    const extensions = autoAcceptAll ? ['*'] : parseExtensions(autoAcceptExtensions)
    try { await invoke('node_set_auto_accept_extensions', { extensions }) }
    catch (error) { console.warn('auto-accept non impostato', error) }
  }
  $: {
    autoAcceptExtensions
    autoAcceptAll
    if (typeof localStorage !== 'undefined') {
      localStorage.setItem(autoAcceptExtKey, autoAcceptExtensions)
      localStorage.setItem(autoAcceptAllKey, autoAcceptAll ? '1' : '0')
    }
    void applyAutoAccept()
  }

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
  let startMinimized = true
  let autostartEnabled = false
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
  let contactRequests: { peerId: string; name: string }[] = []
  const deleteEveryoneWindowMs = 15 * 60 * 1000
  let messageMenu: { message: ChatMessage; x: number; y: number } | null = null
  let contactStatuses: Record<string, string> = {}
  let presenceStatus = 'online'
  let statusMenuOpen = false
  // Composizione IME/tastiera mobile: alcune riportano isComposing male, così
  // l'Invio non spediva e lasciava un a-capo. Tracciamo lo stato esplicitamente.
  let composing = false
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
  // Relay: se il canale diretto col contatto non è pronto ma ho un dispositivo
  // collegato online, posso inviare comunque (verrà inoltrato).
  $: relayAvailable = !activeGroup && Boolean(activeContact) && linkedDevices.some((device) => device.online)
  $: canSend = groupCanSend && (ready || relayAvailable)
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
    let unlistenResize: UnlistenFn | undefined
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
      const syncMaximized = async () => {
        try { document.documentElement.classList.toggle('maximized', await appWindow.isMaximized()) } catch {}
      }
      void syncMaximized()
      void appWindow.onResized(() => void syncMaximized()).then((stop) => unlistenResize = stop)
    }
    if (appWindow) {
      void appWindow.isFocused().then((focused) => windowFocused = focused)
      void appWindow.onFocusChanged(({ payload: focused }) => {
        windowFocused = focused
        if (focused) {
          markActiveConversationRead()
          void appWindow.requestUserAttention(null)
          if (selectedPeerId || selectedGroupId) void focusComposer()
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
      unlistenResize?.()
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
      startMinimized = profile.startMinimized
      try { autostartEnabled = await invoke<boolean>('autostart_get') } catch { autostartEnabled = false }
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
    updateMessage = $t('update.checking')
    try {
      const update = await check({ timeout: 30_000 })
      if (!update) {
        await updateCandidate?.close()
        updateCandidate = null
        updateStatus = 'current'
        updateMessage = $t('update.upToDate')
        if (force) showToast($t('update.upToDateToast'))
        return
      }

      await updateCandidate?.close()
      updateCandidate = update
      updateStatus = 'available'
      updateMessage = $t('update.ready', { version: update.version })
      if (force) showToast($t('update.availableToast', { version: update.version }))
    } catch (error) {
      updateStatus = 'error'
      updateMessage = $t('update.checkFailed', { error: String(error) })
      if (force) showToast($t('update.checkFailedToast'))
      else console.warn('Update check failed', error)
    }
  }

  async function installUpdate() {
    if (!updateCandidate || updateStatus === 'downloading' || updateStatus === 'installing') return
    updateStatus = 'downloading'
    updateProgress = 0
    updateDownloaded = 0
    updateDownloadTotal = 0
    updateMessage = $t('update.downloading', { version: updateCandidate.version })
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
        updateMessage = $t('update.installComplete')
      })
      await relaunch()
    } catch (error) {
      updateStatus = 'error'
      updateMessage = $t('update.failed', { error: String(error) })
      showToast($t('update.failedToast'))
    }
  }

  function lastUpdateCheckLabel() {
    if (!lastUpdateCheck) return $t('update.notChecked')
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
    showToast(durationMs === null ? $t('toast.chatMuted') : $t('toast.chatMutedTemp'))
  }

  async function unmuteConversation(conversation: string) {
    const { [conversation]: _removed, ...remaining } = notificationMutes
    notificationMutes = remaining
    localStorage.setItem(notificationMutesKey, JSON.stringify(notificationMutes))
    closeContextMenu()
    if (running) await invoke('node_set_notification_mute', {
      conversation, muted: false, untilMs: null,
    }).catch((error) => showToast(String(error)))
    showToast($t('toast.notificationsOn'))
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

  function notifyTaskbar(conversation: string, title?: string, body?: string) {
    if (windowFocused || isConversationMuted(conversation)) return
    void appWindow?.requestUserAttention(UserAttentionType.Informational)
    if (title) void osNotify(title, body || '')
  }

  function notificationPreview(kind: string, body: string): string {
    if (kind === 'nudge') return $t('notif.nudge')
    if (kind === 'file') return $t('notif.file')
    const text = body.trim()
    return text.length > 140 ? `${text.slice(0, 140)}…` : text
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
      if (effectsSounds) sounds.signIn()
      void applyAutoAccept()
      if (presenceStatus !== 'online') void invoke('node_set_presence_status', { status: presenceStatus })
      return
    }
    if (event.type === 'contactUpdated') {
      contactRequests = contactRequests.filter((request) => request.peerId !== event.contact.peerId)
      upsertContact(event.contact)
      return
    }
    if (event.type === 'contactRequest') {
      if (!contactRequests.some((request) => request.peerId === event.peerId)) {
        contactRequests = [...contactRequests, { peerId: event.peerId, name: event.name }]
      }
      return
    }
    if (event.type === 'messageDeleted') {
      markMessageDeleted(event.eventId)
      return
    }
    if (event.type === 'contactStatus') {
      contactStatuses = { ...contactStatuses, [event.peerId]: event.status }
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
      showToast($t('toast.newEmoticonFrom', { name: contacts.find((item) => item.peerId === event.peerId)?.name || $t('toast.aContact') }))
      return
    }
    if (event.type === 'contactLink') {
      ownContactLink = event.link
      linkRequested = false
      void QRCode.toDataURL(event.qrLink, {
        width: 1024,
        margin: 4,
        color: { dark: '#10284a', light: '#ffffff' },
      }).then((qr) => ownContactQr = qr).catch((error) => showToast($t('toast.qrFailed', { error: String(error) })))
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
      }).then((qr) => devicePairingQr = qr).catch((error) => showToast($t('toast.qrFailed', { error: String(error) })))
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
        showToast($t('toast.deviceLinked'))
      } else if (event.applied) {
        showToast($t('toast.changesSynced', { count: event.applied }))
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
      showToast($t('toast.fileReceived', { filename: event.filename }))
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
      showToast($t('toast.fileSent', { filename: event.filename }))
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
      showToast($t('toast.sendCancelled'))
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
      showToast($t('toast.emoticonDeleted'))
      return
    }
    if (event.type === 'conversationCleared') {
      conversations = { ...conversations, [event.peerId]: [] }
      showToast($t('toast.historyDeleted'))
      return
    }
    if (event.type === 'contactRemoved') {
      contactRequests = contactRequests.filter((request) => request.peerId !== event.peerId)
      contacts = contacts.filter((contact) => contact.peerId !== event.peerId)
      const { [event.peerId]: _removed, ...remaining } = conversations
      conversations = remaining
      if (selectedPeerId === event.peerId) selectedPeerId = contacts[0]?.peerId || ''
      detailsOpen = false
      showToast($t('toast.contactDeleted'))
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
        showToast($t('toast.groupCreated'))
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
        const groupName = chatGroups.find((group) => group.id === event.message.groupId)?.name || $t('notif.newMessage')
        const author = senderName(next)
        const preview = notificationPreview(event.message.kind, event.message.body)
        notifyTaskbar(groupConversationKey(event.message.groupId), groupName, author ? `${author}: ${preview}` : preview)
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
      showToast($t('toast.fileReceivedGroup', { filename: event.filename }))
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
      showToast($t('toast.groupHistoryDeleted'))
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
      showToast($t('toast.fileExported', { path: event.path }))
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
      id: message.eventId || `${message.timestampMs}-${crypto.randomUUID()}`,
      kind: message.kind === 'nudge'
        ? 'nudge'
        : message.kind === 'file'
          ? 'file'
          : message.direction === 'out'
            ? 'outgoing'
            : 'incoming',
      body: message.body,
      deleted: message.kind === 'deleted',
      eventId: message.eventId,
      timestampMs: message.timestampMs,
      relayed: message.relayed,
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
      const senderName = contacts.find((contact) => contact.peerId === message.peerId)?.name || $t('notif.newMessage')
      notifyTaskbar(conversationKey, senderName, notificationPreview(message.kind, message.body))
    }
    if (message.kind === 'nudge' && !next.mine && !isConversationMuted(conversationKey)) {
      void shakeWindow()
      playNudgeSound()
      if (typeof navigator !== 'undefined' && navigator.vibrate) navigator.vibrate([120, 60, 120])
    } else if (message.direction === 'in' && message.kind !== 'nudge' && effectsSounds && !isConversationMuted(conversationKey)) {
      sounds.messageIn()
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

  async function focusComposer() {
    await tick()
    if (messageEditor && messageEditor.getAttribute('contenteditable') === 'true') {
      messageEditor.focus()
    }
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
    void focusComposer()
  }

  function selectGroup(id: string) {
    selectedGroupId = id
    selectedPeerId = ''
    detailsOpen = false
    rosterOpen = false
    chatGroups = chatGroups.map((group) => group.id === id ? { ...group, unread: 0 } : group)
    scrollMessages()
    void focusComposer()
  }

  function senderName(message: ChatMessage) {
    if (message.mine) return displayName
    if (!activeGroup) return activeContact?.name || $t('helper.contact')
    return contacts.find((contact) => contact.peerId === message.senderPeerId)?.name
      || `${message.senderPeerId?.slice(0, 8) || $t('helper.participant')}…`
  }

  function memberName(memberId: string) {
    if (memberId === peerId) return $t('helper.you', { name: displayName })
    return contacts.find((contact) => contact.peerId === memberId)?.name || `${memberId.slice(0, 8)}…`
  }

  function memberBan(group: GroupChat, memberId: string) {
    return group.bans.find((ban) => ban.peerId === memberId
      && (ban.expiresAtMs === null || ban.expiresAtMs > Date.now()))
  }

  function memberRole(group: GroupChat, memberId: string) {
    if (memberId === group.ownerPeerId) return $t('role.owner')
    if (group.admins.includes(memberId)) return $t('role.admin')
    return $t('role.member')
  }

  function canModerateMember(group: GroupChat, memberId: string) {
    if (memberId === peerId || memberId === group.ownerPeerId) return false
    if (peerId === group.ownerPeerId) return true
    return group.admins.includes(peerId) && !group.admins.includes(memberId)
  }

  function banLabel(ban: GroupBan) {
    if (ban.expiresAtMs === null) return $t('ban.permanent')
    return $t('ban.until', { date: new Date(ban.expiresAtMs).toLocaleString() })
  }

  function deviceStatus(device: LinkedDevice) {
    if (device.online) return $t('device.syncing')
    if (!device.lastSeenMs) return $t('device.neverLinked')
    return $t('device.lastSeen', { date: new Date(device.lastSeenMs).toLocaleString() })
  }

  async function moderateGroup(memberId: string, value: string) {
    if (!selectedGroupId || !value) return
    const [action, duration] = value.split(':')
    if (action === 'permaBan' && !confirm($t('confirm.permaBan', { name: memberName(memberId) }))) return
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
    if (last.kind === 'nudge') return $t('subtitle.nudge')
      if (last.kind === 'file') return `📎 ${last.body}`
      return last.body
    }
    return contact.secure ? $t('subtitle.protected') : contact.online ? $t('subtitle.connecting') : $t('subtitle.offline')
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
          relayAddress, startMinimized,
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
      if (effectsSounds) sounds.signOut()
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
      if (effectsSounds) sounds.messageOut()
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
    if (!selectedGroupId || !confirm('Delete this group chat history?')) return
    await invoke('node_clear_group_conversation', { groupId: selectedGroupId })
      .catch((error) => showToast(String(error)))
  }

  async function deleteChatGroup() {
    if (!selectedGroupId || !confirm('Remove this group chat from this device?')) return
    const id = selectedGroupId
    selectedGroupId = ''
    await invoke('node_delete_chat_group', { groupId: id })
      .catch((error) => showToast(String(error)))
  }

  async function openAttachment(message: ChatMessage) {
    if (!message.attachmentId || !message.attachmentMime) {
      showToast($t('toast.oldFileNoArchive'))
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
      pendingEmoticonAction = $t('emo.created')
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
      pendingEmoticonAction = emoticonToSave.saved ? $t('emo.shortcutUpdated') : $t('emo.saved')
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
    if (!emoticonToSave?.saved || !confirm('Delete this emoticon?')) return
    pendingEmoticonAction = $t('emo.deleting')
    try {
      await invoke('node_delete_emoticon', { assetId: emoticonToSave.assetId })
    } catch (error) {
      pendingEmoticonAction = ''
      showToast(String(error))
    }
  }

  async function setAutostart(enabled: boolean) {
    try {
      await invoke('autostart_set', { enabled })
      autostartEnabled = enabled
    } catch (error) {
      autostartEnabled = !enabled
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
        relayAddress, startMinimized,
      })
      displayName = profile.name
      avatarDataUrl = profile.avatarDataUrl || ''
      localStorage.setItem('msnnext-name', profile.name)
      if (closeAfterSave) profileOpen = false
      showToast(closeAfterSave ? $t('toast.settingsSaved') : $t('toast.avatarUpdated'))
    } catch (error) {
      showToast(String(error))
    }
  }

  async function chooseAvatar() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: 'Profile image', extensions: ['png', 'jpg', 'jpeg', 'webp'] }],
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
    if (!selectedPeerId || !confirm('Delete this chat history?')) return
    try { await invoke('node_clear_conversation', { peerId: selectedPeerId }) }
    catch (error) { showToast(String(error)) }
  }

  async function deleteContact() {
    if (!selectedPeerId || !confirm(`Delete ${activeContact?.name || 'this contact'} and their chat?`)) return
    try { await invoke('node_delete_contact', { peerId: selectedPeerId }) }
    catch (error) { showToast(String(error)) }
  }

  async function importContact() {
    if (!contactLink.trim().startsWith('msnnext://add/')) {
      showToast($t('toast.contactLinkInvalid'))
      return
    }
    try {
      await invoke('node_import_contact', { link: contactLink.trim() })
      contactLink = ''
      showToast($t('toast.contactAdded'))
    } catch (error) {
      showToast(String(error))
    }
  }

  async function scanContactQr() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: 'QR images', extensions: ['png', 'jpg', 'jpeg', 'webp', 'gif'] }],
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
    showToast($t('toast.contactLinkCopied'))
  }

  async function saveContactQr() {
    if (!ownContactQr) return
    const path = await save({
      defaultPath: 'msnnext-contact.png',
      filters: [{ name: 'PNG image', extensions: ['png'] }],
    })
    if (!path) return
    await invoke('save_contact_qr', { path, dataUrl: ownContactQr })
      .then(() => showToast($t('toast.qrSaved')))
      .catch((error) => showToast(String(error)))
  }

  async function shareDevicePairing() {
    if (!running) {
      showToast($t('toast.goOnlineLinkDevice'))
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
      showToast($t('toast.goOnlineLinkThis'))
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
      filters: [{ name: 'QR images', extensions: ['png', 'jpg', 'jpeg', 'webp', 'gif'] }],
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
    showToast($t('toast.deviceCodeCopied'))
  }

  async function saveDevicePairingQr() {
    if (!devicePairingQr) return
    const path = await save({
      defaultPath: 'msnnext-device.png',
      filters: [{ name: 'PNG image', extensions: ['png'] }],
    })
    if (!path) return
    await invoke('save_contact_qr', { path, dataUrl: devicePairingQr })
      .then(() => showToast($t('toast.deviceQrSaved')))
      .catch((error) => showToast(String(error)))
  }

  async function prepareAccountBackupExport() {
    if (running) {
      showToast($t('toast.goOfflineBackup'))
      return
    }
    const path = await save({
      defaultPath: 'msnnext-account.msnnext-account',
      filters: [{ name: 'msnnext account backup', extensions: ['msnnext-account'] }],
    })
    if (!path) return
    accountBackupMode = 'export'
    accountBackupPath = path
    accountBackupPassword = ''
    accountBackupOpen = true
  }

  async function prepareAccountBackupImport() {
    if (running) {
      showToast($t('toast.goOfflineRestore'))
      return
    }
    const path = await open({
      multiple: false,
      directory: false,
      filters: [{ name: 'msnnext account backup', extensions: ['msnnext-account'] }],
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
        showToast($t('toast.accountSaved'))
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
      showToast($t('toast.accountRestored'))
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
    sounds.nudge()
  }

  function showToast(text: string) {
    toastText = text
    clearTimeout(toastTimer)
    toastTimer = setTimeout(() => toastText = '', 3200)
  }

  function openMessageMenu(event: MouseEvent, message: ChatMessage) {
    if (message.deleted || message.kind === 'nudge') return
    event.preventDefault()
    messageMenu = { message, x: event.clientX, y: event.clientY }
  }

  function closeMessageMenu() {
    messageMenu = null
  }

  function canDeleteForEveryone(message: ChatMessage) {
    return message.mine && !!message.eventId && !message.deleted
      && (message.timestampMs === undefined || Date.now() - message.timestampMs < deleteEveryoneWindowMs)
  }

  function markMessageDeleted(eventId: string) {
    const next: Record<string, ChatMessage[]> = {}
    for (const [key, list] of Object.entries(conversations)) {
      next[key] = list.map((message) => message.eventId === eventId
        ? { ...message, deleted: true, body: '', emoticons: [], attachmentId: undefined, attachmentDataUrl: undefined }
        : message)
    }
    conversations = next
  }

  async function deleteMessageForMe(message: ChatMessage) {
    closeMessageMenu()
    const next: Record<string, ChatMessage[]> = {}
    for (const [key, list] of Object.entries(conversations)) {
      next[key] = list.filter((item) => item.id !== message.id)
    }
    conversations = next
    if (!message.eventId) return
    try { await invoke('node_delete_message_for_me', { eventId: message.eventId }) }
    catch (error) { showToast(String(error)) }
  }

  async function deleteMessageForEveryone(message: ChatMessage) {
    closeMessageMenu()
    if (!message.eventId || !selectedPeerId) return
    markMessageDeleted(message.eventId)
    try { await invoke('node_delete_message_for_everyone', { peerId: selectedPeerId, eventId: message.eventId }) }
    catch (error) { showToast(String(error)) }
  }

  async function setPresenceStatus(status: string) {
    presenceStatus = status
    statusMenuOpen = false
    if (!isTauri() || !running) return
    try { await invoke('node_set_presence_status', { status }) }
    catch (error) { showToast(String(error)) }
  }

  async function acceptContactRequest(peerId: string) {
    contactRequests = contactRequests.filter((request) => request.peerId !== peerId)
    try { await invoke('node_accept_contact_request', { peerId }) }
    catch (error) { showToast(String(error)) }
  }

  async function rejectContactRequest(peerId: string) {
    contactRequests = contactRequests.filter((request) => request.peerId !== peerId)
    try { await invoke('node_reject_contact_request', { peerId }) }
    catch (error) { showToast(String(error)) }
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
      {#if updateCandidate}
        <button
          class="titlebar-update"
          disabled={updateStatus === 'downloading' || updateStatus === 'installing'}
          title={$t('update.install', { version: updateCandidate.version })}
          onclick={installUpdate}
        >
          <Download size={14} />
          <span>{updateStatus === 'downloading' ? `${updateProgress || '…'}%` : updateStatus === 'installing' ? $t('update.restarting') : $t('update.updateTo', { version: updateCandidate.version })}</span>
        </button>
      {/if}
      <div class="window-controls">
        <button aria-label={$t('window.minimize')} title={$t('window.minimize')} onclick={() => void appWindow?.minimize()}><Minus size={15} /></button>
        <button aria-label={$t('window.maximize')} title={$t('window.maximize')} onclick={() => void appWindow?.toggleMaximize()}><Square size={12} /></button>
        <button class="window-close" aria-label={$t('window.close')} title={$t('window.close')} onclick={() => void appWindow?.close()}><X size={16} /></button>
      </div>
    </div>
  </header>

  <div class="workspace">
    <aside class="contacts-pane">
      <header class="my-profile">
        <div
          class="avatar-shell me status-trigger"
          data-status={running ? presenceStatus : 'offline'}
          role="button"
          tabindex="0"
          aria-haspopup="menu"
          aria-expanded={statusMenuOpen}
          title={$t('status.change')}
          onclick={() => statusMenuOpen = !statusMenuOpen}
          onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); statusMenuOpen = !statusMenuOpen } }}
        >
          {#if avatarDataUrl}<img src={avatarDataUrl} alt="" />{:else}<span>{displayName.slice(0, 1).toUpperCase()}</span>{/if}
          <i class:online={running} data-status={running ? presenceStatus : 'offline'}></i>
        </div>
        <div class="profile-copy">
          <strong>{displayName}</strong>
          <span>{running ? $t(`status.${presenceStatus}`) : $t('profile.offline')}</span>
        </div>
        <button aria-label={$t('settings.open')} title={$t('settings.open')} onclick={() => openSettings()}>
          <Settings size={17} />
        </button>
        {#if statusMenuOpen}
          <button class="context-scrim" aria-label={$t('ctx.close')} onclick={() => statusMenuOpen = false}></button>
          <div class="status-menu" role="menu">
            {#if running}
              <button role="menuitem" onclick={() => setPresenceStatus('online')}><i class="status-dot" data-status="online"></i>{$t('status.online')}</button>
              <button role="menuitem" onclick={() => setPresenceStatus('busy')}><i class="status-dot" data-status="busy"></i>{$t('status.busy')}</button>
              <button role="menuitem" onclick={() => setPresenceStatus('away')}><i class="status-dot" data-status="away"></i>{$t('status.away')}</button>
              <div class="status-sep"></div>
              <button role="menuitem" class="danger-item" onclick={() => { statusMenuOpen = false; void stopNode() }}>{$t('action.disconnect')}</button>
            {:else}
              <button role="menuitem" onclick={() => { statusMenuOpen = false; void startNode(false) }}>{$t('action.connect')}</button>
            {/if}
          </div>
        {/if}
      </header>

      <div class="roster-actions">
        <label class="search-field">
          <span>{$t('roster.search')}</span>
          <input bind:value={searchQuery} aria-label={$t('roster.search')} placeholder={$t('roster.searchPlaceholder')} />
        </label>
        <button class="add-contact" aria-label={$t('roster.add')} title={$t('roster.add')} onclick={openContacts}>
          <UserRoundPlus size={18} />
        </button>
      </div>

      <section class="contact-list" aria-label={$t('roster.contacts')}>
        {#if chatGroups.length}
          <div class="roster-section-label">{$t('roster.groupChats')}</div>
          {#each chatGroups as group (group.id)}
            <button class:active={group.id === selectedGroupId} class="contact-row group-chat-row" oncontextmenu={(event) => showGroupMenu(event, group)} onclick={() => selectGroup(group.id)}>
              <span class="group-chat-avatar"><UsersRound size={18} /></span>
              <span class="contact-copy"><strong>{group.name}</strong><small>{$t('roster.participants', { count: group.members.length })}</small></span>
              <span class="roster-indicators">{#if isConversationMuted(groupConversationKey(group.id))}<BellOff class="muted-conversation" size={13} />{/if}{#if group.unread}<b class="unread">{group.unread}</b>{/if}</span>
            </button>
          {/each}
        {/if}
        {#if visibleContacts.length}
          <div class="roster-section-label">{$t('roster.contacts')}</div>
          {#each sortedContacts as contact (contact.peerId)}
            <button
              class:active={contact.peerId === selectedPeerId}
              class="contact-row"
              oncontextmenu={(event) => showContactMenu(event, contact)}
              onclick={() => selectContact(contact.peerId)}
            >
              <span class:offline={!contact.online} class="avatar-shell contact-avatar">
                <span>{contact.name.slice(0, 1).toUpperCase()}</span>
                <i class:online={contact.online} data-status={contact.online ? (contactStatuses[contact.peerId] || 'online') : 'offline'}></i>
              </span>
              <span class="contact-copy"><strong>{contact.name}</strong><small>{contactSubtitle(contact)}</small></span>
              <span class="roster-indicators">{#if isConversationMuted(peerConversationKey(contact.peerId))}<BellOff class="muted-conversation" size={13} />{/if}{#if contact.unread}<b class="unread">{contact.unread}</b>{/if}</span>
            </button>
          {/each}
        {:else if contacts.length && !chatGroups.length}
          <div class="empty-contacts compact">
            <strong>{$t('roster.noResults.title')}</strong>
            <p>{$t('roster.noResults.body')}</p>
          </div>
        {:else if !chatGroups.length}
          <div class="empty-contacts">
            <span class="empty-people" aria-hidden="true"><i></i><i></i></span>
            <strong>{$t('roster.empty.title')}</strong>
            <p>{$t('roster.empty.body')}</p>
            <button onclick={openContacts}><Plus size={15} /> {$t('roster.empty.add')}</button>
          </div>
        {/if}
      </section>

      <footer class="roster-footer">
        <button class="icon-only" aria-label={$t('roster.footer.add')} title={$t('roster.footer.add')} onclick={openContacts}><UserRoundPlus size={17} /></button>
        <button class="icon-only" aria-label={$t('roster.footer.newGroup')} title={$t('roster.footer.newGroup')} onclick={openGroupCreation}><UsersRound size={17} /></button>
        <span>{$t('roster.online', { count: onlineContacts.length })}</span>
      </footer>
    </aside>

    <button class="roster-scrim" aria-label={$t('conv.closeList')} onclick={() => rosterOpen = false}></button>

    <section class="conversation">
      <header class="conversation-header">
        <div class="conversation-person">
          <button class="mobile-roster-button" aria-label={$t('conv.openList')} onclick={() => rosterOpen = true}>
            <Menu size={19} />
          </button>
          <div class="avatar-shell large">
            {#if activeGroup}<UsersRound size={20} />{:else}<span>{activeContact?.name.slice(0, 1).toUpperCase() || '?'}</span>{/if}
            {#if activeContact}<i class:online={activeContact.online}></i>{/if}
          </div>
          <span>
            <strong>{activeGroup?.name || activeContact?.name || 'msnnext'}</strong>
            <small>
              {activeGroup ? $t('conv.groupConnected', { count: groupOnline }) : ready ? $t('conv.availableProtected') : activeContact?.online ? $t('conv.preparing') : activeContact ? $t('conv.offline') : $t('conv.choose')}
            </small>
          </span>
        </div>
        <div class="header-actions">
          <span
            class:secure={ready}
            class:waiting={!ready && (activeContact?.online || !!activeGroup)}
            class="security-badge"
            title={ready ? $t('conv.protectedHint') : (activeContact?.online || activeGroup) ? $t('conv.waitingHint') : $t('conv.offlineHint')}
            aria-label={ready ? $t('conv.protected') : (activeContact?.online || activeGroup) ? $t('conv.waiting') : $t('conv.offline')}
          >
            {#if ready}<ShieldCheck size={16} />{:else if activeContact?.online || activeGroup}<ShieldAlert size={16} />{:else}<ShieldOff size={16} />{/if}
          </span>
          <button class:active={detailsOpen} class="header-tool" aria-label={$t('conv.details')} title={$t('conv.details')} onclick={openConversationDetails}>
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
              <p class="welcome-kicker">{$t('welcome.kicker')}</p>
              <h1>{$t('welcome.title1')}<br />{$t('welcome.title2')}</h1>
              <p>{$t('welcome.body')}</p>
              <button class="primary-button" onclick={running ? openContacts : () => setupOpen = true}>
                {running ? $t('welcome.addContact') : $t('welcome.goOnline')}
              </button>
            </div>
          {:else if messages.length === 0}
            <div class="conversation-empty">
              <div class="avatar-shell hero-avatar">
                {#if activeGroup}<UsersRound size={30} />{:else}<span>{activeContact?.name.slice(0, 1).toUpperCase()}</span><i class:online={activeContact?.online}></i>{/if}
              </div>
              <h2>{activeGroup?.name || activeContact?.name}</h2>
              <p>{activeGroup ? (ready ? $t('convEmpty.groupReady') : $t('convEmpty.groupWait')) : ready ? $t('convEmpty.ready') : $t('convEmpty.offline')}</p>
            </div>
          {:else}
            <div class="session-start"><span>{$t('msg.sessionStart')}</span></div>
            {#each messages as message, index (message.id)}
              {#if message.kind === 'nudge'}
                <div class="nudge-message">
                  <span><Zap size={18} /></span>
                  <p><strong>{message.mine ? $t('msg.youNudged') : $t('msg.contactNudged', { name: activeContact?.name || $t('msg.aContact') })}</strong><small>{$t('msg.nudgeShake')}</small></p>
                  <time>{message.time}</time>
                </div>
              {:else}
                {@const prev = messages[index - 1]}
                {@const sameSender = !!prev && prev.kind !== 'nudge' && prev.mine === message.mine && (prev.senderPeerId || '') === (message.senderPeerId || '')}
                <article class:mine={message.mine} class:file-message={message.kind === 'file'} class:deleted={message.deleted} class:continued={sameSender} class="message-line" oncontextmenu={(event) => openMessageMenu(event, message)}>
                  <header>
                    {#if !sameSender}<strong>{senderName(message)}</strong>{/if}
                    <time>{message.time}</time>
                    {#if message.relayed}<span class="relayed-badge" title={$t('msg.relayedHint')}><Share2 size={11} /></span>{/if}
                  </header>
                  {#if message.deleted}
                    <p class="deleted-message"><Trash2 size={13} /> {message.mine ? $t('msg.deletedByYou') : $t('msg.deletedMessage')}</p>
                  {:else if message.kind === 'file'}
                    <button class="file-line" disabled={!message.attachmentId} onclick={() => openAttachment(message)}>
                      {#if message.attachmentDataUrl}
                        <img src={message.attachmentDataUrl} alt={message.body} />
                      {:else}<Paperclip size={17} />{/if}
                      <span><b>{message.attachmentMime?.startsWith('image/') ? (message.mine ? $t('msg.imageSent') : $t('msg.imageReceived')) : (message.mine ? $t('msg.fileSent') : $t('msg.fileReceived'))}</b><small>{message.body}</small></span>
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
            <header><strong>{$t('details.title')}</strong><button aria-label={$t('details.close')} onclick={() => detailsOpen = false}><X size={17} /></button></header>
            <div class="detail-profile">
              <div class="avatar-shell profile-avatar">
                {#if activeGroup}<UsersRound size={25} />{:else}<span>{activeContact?.name.slice(0, 1).toUpperCase() || displayName.slice(0, 1).toUpperCase()}</span><i class:online={activeContact?.online || running}></i>{/if}
              </div>
              <strong>{activeGroup?.name || activeContact?.name || displayName}</strong>
              <small>{activeGroup ? $t('roster.participants', { count: activeGroup.members.length }) : activeContact ? (activeContact.online ? $t('profile.available') : $t('profile.offline')) : (running ? $t('details.online') : $t('profile.offline'))}</small>
            </div>
            <section class="detail-section">
              <h3>{$t('details.security')}</h3>
              <div class="detail-row">
                <span><ShieldCheck size={18} /></span>
                <p><strong>{$t('details.protectedConv')}</strong><small>{ready ? $t('details.protectedOn') : $t('details.protectedOff')}</small></p>
                <i class:active={ready}></i>
              </div>
              <div class="detail-row">
                <span><Activity size={18} /></span>
                <p><strong>{$t('details.directConn')}</strong><small>{activeContact?.online ? $t('details.directOn') : $t('details.directOff')}</small></p>
                <i class:active={activeContact?.online}></i>
              </div>
              <details class="technical-details">
                <summary>{$t('details.technical')}</summary>
                <p>{$t('details.crypto')}</p>
              </details>
            </section>
            <section class="detail-section identity-detail">
              <h3>{activeContact ? $t('details.contactIdentity') : $t('details.yourIdentity')}</h3>
              <code>{activeContact?.fingerprint || ownFingerprint || $t('details.identityAfterStart')}</code>
              <small>{$t('details.compareIdentity')}</small>
              <button disabled={!running || linkRequested} onclick={openContacts}><QrCode size={15} /> {$t('details.showQr')}</button>
            </section>
            {#if activeContact}
              <section class="detail-section contact-management">
                <h3>{$t('details.contactManagement')}</h3>
                <div class="contact-name-editor">
                  <label for="contact-display-name">{$t('details.displayName')}</label>
                  <div class="contact-name-row"><input id="contact-display-name" bind:value={contactName} maxlength="64" placeholder={activeContact.name} /><button onclick={renameContact}><Pencil size={14} /> {$t('details.save')}</button></div>
                </div>
                <div class="contact-danger-zone">
                  <button onclick={clearConversation}><Trash2 size={14} /><span><strong>{$t('details.clearHistory')}</strong><small>{$t('details.clearHistoryHint')}</small></span></button>
                  <button class="danger-button" onclick={deleteContact}><Trash2 size={14} /><span><strong>{$t('details.removeContact')}</strong><small>{$t('details.removeContactHint')}</small></span></button>
                </div>
              </section>
            {/if}
            {#if activeGroup}
              <section class="detail-section group-management">
                <h3>{$t('details.participantsTitle')}</h3>
                <ul>
                  {#each activeGroup.members as member}
                    {@const ban = memberBan(activeGroup, member)}
                    <li class="group-member-row">
                      <span class="group-member-copy">
                        <strong>{memberName(member)}</strong>
                        <small>
                          <b>{memberRole(activeGroup, member)}</b>
                          {#if activeGroup.silenced.includes(member)}<i>{$t('details.muted')}</i>{/if}
                          {#if ban}<i class="ban-status">{banLabel(ban)}</i>{/if}
                        </small>
                      </span>
                      {#if canModerateMember(activeGroup, member)}
                        <select aria-label={$t('details.manage', { name: memberName(member) })} value="" onchange={(event) => moderateGroup(member, event.currentTarget.value)}>
                          <option value="">{$t('details.manageOpt')}</option>
                          {#if peerId === activeGroup.ownerPeerId}
                            <option value={activeGroup.admins.includes(member) ? 'member' : 'admin'}>{activeGroup.admins.includes(member) ? $t('details.makeMember') : $t('details.makeAdmin')}</option>
                          {/if}
                          {#if !activeGroup.admins.includes(member)}
                            {#if ban}
                              <option value="unban">{$t('details.removeBan')}</option>
                            {:else}
                              <option value={activeGroup.silenced.includes(member) ? 'unsilence' : 'silence'}>{activeGroup.silenced.includes(member) ? $t('details.unmute') : $t('details.mute')}</option>
                              <option value="tempBan:3600000">{$t('details.ban1h')}</option>
                              <option value="tempBan:86400000">{$t('details.ban24h')}</option>
                              <option value="tempBan:604800000">{$t('details.ban7d')}</option>
                              <option value="permaBan">{$t('details.banPerma')}</option>
                            {/if}
                          {/if}
                        </select>
                      {/if}
                    </li>
                  {/each}
                </ul>
                <button onclick={clearGroupConversation}><Trash2 size={14} /> {$t('details.deleteHistory')}</button>
                <button class="danger-button" onclick={deleteChatGroup}><Trash2 size={14} /> {$t('details.removeGroup')}</button>
              </section>
            {/if}
            <div class="privacy-note"><LockKeyhole size={14} /><span>{$t('details.privacy')}</span></div>
          </aside>
        {/if}
      </div>

      <footer class="composer-wrap">
        {#if emojiOpen}
          <div class="emoji-picker">
            <header>
              <span><strong>{$t('emoji.title')}</strong><small>{$t('emoji.subtitle')}</small></span>
              <span class="emoji-header-actions">
                <button class="create-emoticon-button" onclick={chooseEmoticonFile}><Plus size={14} /> {$t('emoji.create')}</button>
                <button aria-label={$t('emoji.close')} onclick={() => emojiOpen = false}><X size={15} /></button>
              </span>
            </header>
            {#if customEmoticons.length}
              <small class="emoji-section-label">{$t('emoji.yours')}</small>
              <div class="emoji-grid custom-emoji-grid">
                {#each customEmoticons as item (item.assetId)}
                  <div class="custom-emoji-item">
                    <button aria-label={$t('emoji.insert', { name: item.name })} title={`${item.name} · ${item.trigger}`} onclick={() => insertCustomEmoticon(item)}>
                      <img src={item.dataUrl} alt="" /><small>{item.trigger}</small>
                    </button>
                    <button class="edit-emoticon" aria-label={$t('emoji.edit', { name: item.name })} title={$t('emoji.editOrDelete')} onclick={() => openSaveEmoticon(item)}><Pencil size={11} /></button>
                  </div>
                {/each}
              </div>
            {/if}
            {#if offeredEmoticons.length}
              <small class="emoji-section-label">{$t('emoji.received')}</small>
              <div class="received-emoji-list">
                {#each offeredEmoticons as item (item.assetId)}
                  <div><img src={item.dataUrl} alt={item.name} /><span><strong>{item.name}</strong><small>{item.trigger}</small></span><button onclick={() => openSaveEmoticon(item)}>{$t('emoji.saveShort')}</button></div>
                {/each}
              </div>
            {/if}
            <small class="emoji-section-label">{$t('emoji.classic')}</small>
            <div class="emoji-grid">
              {#each emoticons as item}
                <button aria-label={$t('emoji.insert', { name: item.label })} title={`${item.label} · ${item.shortcut}`} onclick={() => insertEmoticon(item)}>
                  <span>{item.glyph}</span><small>{item.shortcut}</small>
                </button>
              {/each}
            </div>
            <p>{$t('emoji.hint')}</p>
          </div>
        {/if}

        <div class="chat-toolbar" aria-label={$t('toolbar.label')}>
          <button class:active={emojiOpen} disabled={!canSend} onclick={() => emojiOpen = !emojiOpen}><Smile size={18} /><span>{$t('toolbar.emoticon')}</span></button>
          <button class="nudge-tool" disabled={!ready || !!activeGroup} onclick={sendNudge}><Zap size={18} /><span>{$t('composer.nudge')}</span></button>
          <button disabled={!canSend || fileSending} onclick={chooseFile}><Paperclip size={18} /><span>{fileSending ? $t('composer.sending') : $t('composer.sendFile')}</span></button>
          {#if fileSending}<button class="danger-item" onclick={cancelFileTransfers}><X size={18} /><span>{$t('composer.cancel')}</span></button>{/if}
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
            aria-label={$t('composer.message')}
            aria-multiline="true"
            aria-disabled={!canSend}
            data-placeholder={!groupCanSend ? $t('composer.cantWrite') : ready ? $t('composer.writeTo', { name: activeGroup?.name || activeContact?.name || '' }) : relayAvailable ? $t('composer.relayHint') : activeGroup ? $t('composer.noParticipants') : activeContact ? $t('composer.contactUnavailable') : $t('composer.chooseConv')}
            oninput={syncDraft}
            onpaste={pasteDraft}
            ondrop={(event) => event.preventDefault()}
            oncompositionstart={() => composing = true}
            oncompositionend={() => composing = false}
            onkeydown={(event) => {
              if (event.key === 'Enter' && !composing && !event.isComposing) {
                event.preventDefault()
                if (event.shiftKey) {
                  insertAtDraftCaret('\n')
                  syncDraft()
                } else void sendMessage()
              }
            }}
          ></div>
          <button type="submit" class="send-button" disabled={!canSend || !messageText.trim()}><Send size={17} /> {$t('composer.send')}</button>
        </form>
        <small class="composer-hint">{$t('composer.hint')}</small>
      </footer>
      {#if fileDropActive && canSend}
        <div class="file-drop-overlay"><Paperclip size={28} /><strong>{$t('composer.dropToSend')}</strong><small>{$t('composer.dropHint')}</small></div>
      {/if}
    </section>
  </div>
</main>

{#if contextPeerId || contextGroupId}
  {@const contextConversation = contextGroupId ? groupConversationKey(contextGroupId) : peerConversationKey(contextPeerId)}
  <button class="context-scrim" aria-label={$t('ctx.close')} onclick={closeContextMenu}></button>
  <div class="contact-context-menu" style={`left:${contextX}px;top:${contextY}px`} role="menu">
    <button onclick={() => { contextGroupId ? selectGroup(contextGroupId) : selectContact(contextPeerId); closeContextMenu() }}>{$t('ctx.open')}</button>
    {#if contextPeerId}<button onclick={() => manageContact(contextPeerId)}>{$t('ctx.rename')}</button>{/if}
    <div class="context-separator"></div>
    <small>{$t('ctx.notifications')}</small>
    {#if isConversationMuted(contextConversation)}
      <button onclick={() => unmuteConversation(contextConversation)}>{$t('ctx.unmute')}</button>
    {:else}
      <button onclick={() => muteConversation(contextConversation, 60 * 60 * 1000)}>{$t('ctx.mute1h')}</button>
      <button onclick={() => muteConversation(contextConversation, 8 * 60 * 60 * 1000)}>{$t('ctx.mute8h')}</button>
      <button onclick={() => muteConversation(contextConversation, null)}>{$t('ctx.muteAlways')}</button>
    {/if}
    {#if contextPeerId}
    <div class="context-separator"></div>
    <button class="danger-item" onclick={() => { selectContact(contextPeerId); closeContextMenu(); void deleteContact() }}>{$t('ctx.deleteContact')}</button>
    {/if}
  </div>
{/if}

{#if mediaPreview}
  <div class="modal-backdrop image-viewer" role="dialog" aria-modal="true" aria-label={$t('media.previewLabel')} tabindex="-1">
    <button aria-label={$t('media.close')} onclick={() => mediaPreview = ''}><X size={20} /></button>
    {#if mediaPreview.startsWith('data:video/')}
      <!-- svelte-ignore a11y_media_has_caption i file ricevuti non includono una traccia sottotitoli separata -->
      <video src={mediaPreview} controls autoplay aria-label={$t('media.video')}></video>
    {:else}
      <img src={mediaPreview} alt={$t('media.image')} />
    {/if}
  </div>
{/if}

{#if incomingAttachmentOffers[0]}
  {@const offer = incomingAttachmentOffers[0]}
  <div class="modal-backdrop">
    <div class="modal" role="dialog" aria-modal="true" aria-labelledby="attachment-offer-title">
      <p class="step-label">{$t('offer.incoming')}</p>
      <h2 id="attachment-offer-title">{$t('offer.question', { filename: offer.filename })}</h2>
      <p>{$t('offer.body', { name: contacts.find((contact) => contact.peerId === offer.peerId)?.name || $t('offer.aContact'), size: formatBytes(offer.size) })}</p>
      <div class="modal-actions">
        <button class="secondary-button" onclick={() => answerAttachmentOffer(offer, false)}>{$t('offer.decline')}</button>
        <button class="primary-button" onclick={() => answerAttachmentOffer(offer, true)}>{$t('offer.accept')}</button>
      </div>
    </div>
  </div>
{/if}

{#if setupOpen}
  <div class="modal-backdrop">
    <div class="modal-theme-switcher" role="group" aria-label={$t('modal.windowTheme')}>
      <button class:active={theme === 'light'} onclick={() => setTheme('light')}><Sun size={14} /> {$t('theme.light')}</button>
      <button class:active={theme === 'dark'} onclick={() => setTheme('dark')}><Moon size={14} /> {$t('theme.dark')}</button>
      <button class:active={theme === 'system'} onclick={() => setTheme('system')}><Monitor size={14} /> {$t('theme.system')}</button>
    </div>
    <div class="modal setup-modal" role="dialog" aria-modal="true" aria-labelledby="setup-title">
      {#if running}<button class="modal-close" aria-label={$t('window.close')} onclick={() => setupOpen = false}><X size={18} /></button>{/if}
      <div class="modal-sky">
        <span class="modal-people" aria-hidden="true"><i></i><i></i></span>
        <div><strong>msnnext</strong><small>messenger</small></div>
      </div>
      <div class="modal-body">
        <p class="step-label">{$t('setup.step')}</p>
        <h2 id="setup-title">{$t('setup.title')}</h2>
        <p>{$t('setup.body')}</p>
        <label>{$t('setup.yourName')}<input bind:value={displayName} maxlength="64" placeholder={$t('setup.namePlaceholder')} /></label>
        <details>
          <summary>{$t('setup.advanced')}</summary>
          <label>{$t('setup.peerAddress')} <small>{$t('setup.optional')}</small><input bind:value={directAddress} placeholder="/ip4/…/udp/…/quic-v1/p2p/…" /></label>
          <label>{$t('setup.customRelay')} <small>{$t('setup.optional')}</small><input bind:value={relayAddress} maxlength="512" placeholder={$t('setup.relayPlaceholder')} /></label>
        </details>
        <button class="primary-button wide" disabled={starting || !displayName.trim()} onclick={() => startNode()}>
          {starting ? $t('setup.connecting') : $t('setup.goOnline')}
        </button>
        <button class="secondary-button wide" onclick={prepareAccountBackupImport}><Upload size={14} /> {$t('setup.restore')}</button>
        <small class="modal-foot">{$t('setup.foot')}</small>
      </div>
    </div>
  </div>
{/if}

{#if connectOpen}
  <div class="modal-backdrop">
    <div class="modal-theme-switcher" role="group" aria-label={$t('modal.windowTheme')}>
      <button class:active={theme === 'light'} onclick={() => setTheme('light')}><Sun size={14} /> {$t('theme.light')}</button>
      <button class:active={theme === 'dark'} onclick={() => setTheme('dark')}><Moon size={14} /> {$t('theme.dark')}</button>
      <button class:active={theme === 'system'} onclick={() => setTheme('system')}><Monitor size={14} /> {$t('theme.system')}</button>
    </div>
    <div class="modal connect-modal" role="dialog" aria-modal="true" aria-labelledby="connect-title">
      <button class="modal-close" aria-label={$t('window.close')} onclick={() => connectOpen = false}><X size={18} /></button>
      <div class="modal-heading">
        <span><UserRoundPlus size={23} /></span>
        <div><p class="step-label">{$t('connect.step')}</p><h2 id="connect-title">{$t('connect.title')}</h2></div>
      </div>
      <p>{$t('connect.body')}</p>

      <section class="share-section">
        <header><span><strong>{$t('connect.yourContact')}</strong><small>{$t('connect.yourContactHint')}</small></span><QrCode size={19} /></header>
        {#if ownContactQr}
          <img class="contact-qr" src={ownContactQr} alt={$t('connect.qrAlt')} />
        {:else}
          <button class="secondary-button" disabled={linkRequested} onclick={createContactLink}>{linkRequested ? $t('connect.preparingQr') : $t('connect.createQr')}</button>
        {/if}
        {#if ownContactLink}
          <div class="contact-share-actions">
            <button class="copy-link" onclick={copyOwnLink}><Copy size={15} /> {$t('connect.copyLink')}</button>
            <button class="copy-link" onclick={saveContactQr}><QrCode size={15} /> {$t('connect.saveQr')}</button>
          </div>
        {/if}
      </section>

      <div class="or-divider"><span>{$t('connect.orAdd')}</span></div>
      <label>{$t('connect.receivedLink')}<input bind:value={contactLink} placeholder="msnnext://add/…" /></label>
      <button class="scan-button" onclick={scanContactQr}><QrCode size={16} /> {$t('connect.scanQr')}</button>
      <button class="primary-button wide" disabled={!running || !contactLink.trim()} onclick={importContact}>
        <Link2 size={16} /> {$t('connect.addToList')}
      </button>
    </div>
  </div>
{/if}

{#if groupCreateOpen}
  <div class="modal-backdrop">
    <div class="modal group-create-modal" role="dialog" aria-modal="true" aria-labelledby="group-create-title">
      <button class="modal-close" aria-label={$t('window.close')} onclick={() => groupCreateOpen = false}><X size={18} /></button>
      <div class="modal-heading">
        <span><UsersRound size={23} /></span>
        <div><p class="step-label">{$t('group.step')}</p><h2 id="group-create-title">{$t('group.title')}</h2></div>
      </div>
      <label>{$t('group.name')}<input bind:value={groupName} maxlength="64" placeholder={$t('group.namePlaceholder')} /></label>
      <fieldset class="group-member-picker">
        <legend>{$t('group.pickPeople')}</legend>
        {#each contacts as contact (contact.peerId)}
          <label>
            <input type="checkbox" checked={groupMemberIds.includes(contact.peerId)} onchange={() => toggleGroupMember(contact.peerId)} />
            <span><strong>{contact.name}</strong><small>{contact.secure ? $t('group.onlineProtected') : contact.online ? $t('group.connecting') : $t('group.offline')}</small></span>
          </label>
        {/each}
      </fieldset>
      <button class="primary-button wide" disabled={!groupName.trim() || groupMemberIds.length < 2 || pendingGroupCreation} onclick={createChatGroup}>{pendingGroupCreation ? $t('group.creating') : $t('group.createWith', { count: groupMemberIds.length + 1 })}</button>
    </div>
  </div>
{/if}

{#if emoticonCreateOpen}
  <div class="modal-backdrop">
    <div class="modal emoticon-modal" role="dialog" aria-modal="true" aria-labelledby="create-emoticon-title">
      <button class="modal-close" aria-label={$t('window.close')} onclick={() => emoticonCreateOpen = false}><X size={18} /></button>
      <div class="modal-heading">
        <span><Smile size={23} /></span>
        <div><p class="step-label">{$t('emoCreate.step')}</p><h2 id="create-emoticon-title">{$t('emoCreate.title')}</h2></div>
      </div>
      <p>{$t('emoCreate.body')} <b>:ciao:</b>.</p>
      <label>{$t('emoCreate.shortcut')}<input bind:value={emoticonTrigger} maxlength="32" placeholder=":mia:" /></label>
      <button class="primary-button wide" disabled={!emoticonTrigger.trim()} onclick={createCustomEmoticon}>{$t('emoCreate.create')}</button>
    </div>
  </div>
{/if}

{#if emoticonSaveOpen && emoticonToSave}
  <div class="modal-backdrop">
    <div class="modal emoticon-modal" role="dialog" aria-modal="true" aria-labelledby="save-emoticon-title">
      <button class="modal-close" aria-label={$t('window.close')} onclick={() => emoticonSaveOpen = false}><X size={18} /></button>
      <div class="received-emoticon-preview"><img src={emoticonToSave.dataUrl} alt={emoticonToSave.name} /></div>
      <p class="step-label">{emoticonToSave.saved ? $t('emoSave.yours') : $t('emoSave.received')}</p>
      <h2 id="save-emoticon-title">{emoticonToSave.saved ? $t('emoSave.edit', { name: emoticonToSave.name }) : $t('emoSave.save', { name: emoticonToSave.name })}</h2>
      <p>{emoticonToSave.saved ? $t('emoSave.changeHint') : $t('emoSave.keepHint')}</p>
      <label>{$t('emoCreate.shortcut')}<input bind:value={emoticonTrigger} maxlength="32" placeholder=":emoticon:" /></label>
      <button class="primary-button wide" disabled={!emoticonTrigger.trim() || !!pendingEmoticonAction} onclick={saveReceivedEmoticon}>{emoticonToSave.saved ? $t('emoSave.saveEdit') : $t('emoSave.saveToMine')}</button>
      {#if emoticonToSave.saved}<button class="danger-button wide" disabled={!!pendingEmoticonAction} onclick={deleteEmoticon}><Trash2 size={15} /> {$t('emoSave.delete')}</button>{/if}
    </div>
  </div>
{/if}

{#if profileOpen}
  <div class="modal-backdrop">
    <div class="modal settings-modal" role="dialog" aria-modal="true" aria-labelledby="settings-title">
      <button class="modal-close" aria-label={$t('window.close')} onclick={() => profileOpen = false}><X size={18} /></button>
      <header class="settings-header">
        <div class="settings-avatar avatar-shell">
          {#if avatarDataUrl}<img src={avatarDataUrl} alt="Avatar personale" />{:else}<span>{displayName.slice(0, 1).toUpperCase()}</span>{/if}
        </div>
        <div>
          <p class="step-label">msnnext {appVersion}</p>
          <h2 id="settings-title">{$t('settings.title')}</h2>
          <small>{displayName}</small>
        </div>
      </header>

      <div class="settings-shell">
        <nav class="settings-navigation" aria-label={$t('settings.sections')}>
          <button class:active={settingsSection === 'profile'} aria-current={settingsSection === 'profile' ? 'page' : undefined} onclick={() => settingsSection = 'profile'}><UserRound size={17} /><span>{$t('settings.nav.profile')}</span></button>
          <button class:active={settingsSection === 'appearance'} aria-current={settingsSection === 'appearance' ? 'page' : undefined} onclick={() => settingsSection = 'appearance'}><Palette size={17} /><span>{$t('settings.nav.appearance')}</span></button>
          <button class:active={settingsSection === 'devices'} aria-current={settingsSection === 'devices' ? 'page' : undefined} onclick={() => settingsSection = 'devices'}><Monitor size={17} /><span>{$t('settings.nav.devices')}</span></button>
          <button class:active={settingsSection === 'data'} aria-current={settingsSection === 'data' ? 'page' : undefined} onclick={() => settingsSection = 'data'}><Database size={17} /><span>{$t('settings.nav.data')}</span></button>
          <button class:active={settingsSection === 'updates'} aria-current={settingsSection === 'updates' ? 'page' : undefined} onclick={() => settingsSection = 'updates'}>
            <RefreshCw size={17} /><span>{$t('settings.nav.updates')}</span>{#if updateCandidate}<i aria-label={$t('settings.updateAvailable')}></i>{/if}
          </button>
          <button class:active={settingsSection === 'network'} aria-current={settingsSection === 'network' ? 'page' : undefined} onclick={() => settingsSection = 'network'}><Radio size={17} /><span>{$t('settings.nav.network')}</span></button>
        </nav>

        <div class="settings-content">
          {#if settingsSection === 'profile'}
            <section class="settings-panel" aria-labelledby="profile-panel-title">
              <header class="settings-panel-heading">
                <h3 id="profile-panel-title">{$t('settings.profile.title')}</h3>
                <p>{$t('settings.profile.body')}</p>
              </header>
              <div class="profile-settings-editor">
                <div class="profile-editor-avatar avatar-shell">
                  {#if avatarDataUrl}<img src={avatarDataUrl} alt={$t('settings.profile.picture')} />{:else}<span>{displayName.slice(0, 1).toUpperCase()}</span>{/if}
                </div>
                <div class="profile-avatar-copy">
                  <strong>{$t('settings.profile.picture')}</strong>
                  <small>{$t('settings.profile.formats')}</small>
                  <div class="profile-avatar-actions">
                    <button class="secondary-button" onclick={chooseAvatar}>{$t('settings.profile.choose')}</button>
                    {#if avatarDataUrl}<button class="secondary-button" onclick={() => saveProfile(null, true, false)}>{$t('settings.profile.remove')}</button>{/if}
                  </div>
                </div>
              </div>
              <label class="settings-field">{$t('settings.profile.displayName')}<input bind:value={displayName} maxlength="64" /></label>
            </section>
          {:else if settingsSection === 'appearance'}
            <section class="settings-panel" aria-labelledby="appearance-panel-title">
              <header class="settings-panel-heading">
                <h3 id="appearance-panel-title">{$t('settings.appearance.title')}</h3>
                <p>{$t('settings.appearance.body')}</p>
              </header>
              <div class="settings-theme-control" role="group" aria-label={$t('settings.appearance.appTheme')}>
                <button class:active={theme === 'light'} onclick={() => setTheme('light')}><Sun size={16} /> {$t('theme.light')}</button>
                <button class:active={theme === 'dark'} onclick={() => setTheme('dark')}><Moon size={16} /> {$t('theme.dark')}</button>
                <button class:active={theme === 'system'} onclick={() => setTheme('system')}><Monitor size={16} /> {$t('theme.system')}</button>
              </div>
              <div class="settings-list">
                <label class="settings-row"><span><strong>{$t('settings.textSize.title')}</strong><small>{$t('settings.textSize.hint')}</small></span><select bind:value={fontScale} aria-label={$t('settings.textSize.title')}><option value={100}>{$t('settings.textSize.original')}</option><option value={115}>{$t('settings.textSize.comfortable')}</option><option value={125}>{$t('settings.textSize.large')}</option><option value={140}>{$t('settings.textSize.xlarge')}</option></select></label>
                <label class="settings-row"><span><strong>{$t('settings.sentImages.title')}</strong><small>{$t('settings.sentImages.desc')}</small></span><input type="checkbox" bind:checked={previewSentImages} /></label>
                <label class="settings-row"><span><strong>{$t('settings.recvImages.title')}</strong><small>{$t('settings.recvImages.desc')}</small></span><input type="checkbox" bind:checked={previewReceivedImages} /></label>
                <label class="settings-row"><span><strong>{$t('settings.nudgeSound.title')}</strong><small>{$t('settings.nudgeSound.desc')}</small></span><input type="checkbox" bind:checked={nudgeSound} /></label>
                <label class="settings-row"><span><strong>{$t('settings.effectsSounds.title')}</strong><small>{$t('settings.effectsSounds.desc')}</small></span><input type="checkbox" bind:checked={effectsSounds} /></label>
                <label class="settings-row"><span><strong>{$t('settings.notifications.title')}</strong><small>{$t('settings.notifications.desc')}</small></span><input type="checkbox" bind:checked={notificationsEnabled} onchange={() => { if (notificationsEnabled) void ensureNotifPermission() }} /></label>
                <label class="settings-row"><span><strong>{$t('settings.autostart.title')}</strong><small>{$t('settings.autostart.desc')}</small></span><input type="checkbox" checked={autostartEnabled} onchange={(e) => void setAutostart(e.currentTarget.checked)} /></label>
                <label class="settings-row"><span><strong>{$t('settings.startMinimized.title')}</strong><small>{$t('settings.startMinimized.desc')}</small></span><input type="checkbox" bind:checked={startMinimized} /></label>
                <label class="settings-row"><span><strong>{$t('settings.language.title')}</strong><small>{$t('settings.language.desc')}</small></span>
                  <select bind:value={$locale}>
                    {#each availableLocales as lang (lang.code)}<option value={lang.code}>{lang.label}</option>{/each}
                  </select>
                </label>
              </div>
            </section>
          {:else if settingsSection === 'devices'}
            <section class="settings-panel" aria-labelledby="linked-devices-title">
              <header class="settings-panel-heading">
                <h3 id="linked-devices-title">{$t('settings.devices.title')}</h3>
                <p>{$t('settings.devices.body')}</p>
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
                <div class="settings-empty"><Monitor size={22} /><strong>{$t('settings.devices.onlyThis.title')}</strong><small>{$t('settings.devices.onlyThis.desc')}</small></div>
              {/if}
              <div class="settings-action-row">
                <button class="secondary-button" disabled={!running} onclick={shareDevicePairing}><QrCode size={14} /> {$t('settings.devices.showCode')}</button>
                <button class="secondary-button" disabled={!running} onclick={joinDevicePairing}><Link2 size={14} /> {$t('settings.devices.useCode')}</button>
              </div>
            </section>
          {:else if settingsSection === 'data'}
            <section class="settings-panel" aria-labelledby="data-panel-title">
              <header class="settings-panel-heading">
                <h3 id="data-panel-title">{$t('settings.data.title')}</h3>
                <p>{$t('settings.data.body')}</p>
              </header>
              <div class="settings-subsection">
                <div class="settings-subsection-heading"><span><strong>{$t('settings.data.backup.title')}</strong><small>{$t('settings.data.backup.desc')}</small></span><ShieldCheck size={18} /></div>
                <div class="settings-action-row">
                  <button class="secondary-button" disabled={running} onclick={prepareAccountBackupExport}><Download size={14} /> {$t('settings.data.export')}</button>
                  <button class="secondary-button" disabled={running} onclick={prepareAccountBackupImport}><Upload size={14} /> {$t('settings.data.import')}</button>
                </div>
                {#if running}<p class="settings-note">{$t('settings.data.offlineNote')}</p>{/if}
              </div>
              <div class="settings-subsection">
                <div class="settings-subsection-heading"><span><strong>{$t('settings.autoAccept.title')}</strong><small>{$t('settings.autoAccept.desc')}</small></span><Download size={18} /></div>
                <label class="settings-row"><span><strong>{$t('settings.autoAccept.all')}</strong><small>{$t('settings.autoAccept.allDesc')}</small></span><input type="checkbox" bind:checked={autoAcceptAll} /></label>
                <label class="settings-field">{$t('settings.autoAccept.label')}<input bind:value={autoAcceptExtensions} placeholder="jpg, png, gif, webp, mp4" disabled={autoAcceptAll} /><small>{$t('settings.autoAccept.hint')}</small></label>
              </div>
              <div class="settings-subsection settings-emoticons-flat">
                <div class="settings-subsection-heading">
                  <span><strong>{$t('settings.data.emoticons.title')}</strong><small>{$t('settings.data.emoticons.desc')}</small></span>
                  <button class="secondary-button" onclick={chooseEmoticonFile}><Plus size={14} /> {$t('emoji.create')}</button>
                </div>
                {#if customEmoticons.length}
                  <div class="emoji-grid custom-emoji-grid">
                    {#each customEmoticons as item (item.assetId)}
                      <button aria-label={$t('emoji.edit', { name: item.name })} title={$t('emoji.editOrDelete')} onclick={() => openSaveEmoticon(item)}><img src={item.dataUrl} alt="" /><small>{item.trigger}</small></button>
                    {/each}
                  </div>
                {:else}
                  <div class="settings-empty compact"><Smile size={20} /><strong>{$t('settings.data.emoticons.none')}</strong></div>
                {/if}
                {#if offeredEmoticons.length}
                  <small class="emoji-section-label">{$t('emoji.received')}</small>
                  <div class="received-emoji-list">
                    {#each offeredEmoticons as item (item.assetId)}
                      <div><img src={item.dataUrl} alt={item.name} /><span><strong>{item.name}</strong><small>{item.trigger}</small></span><button onclick={() => openSaveEmoticon(item)}>{$t('emoji.saveShort')}</button></div>
                    {/each}
                  </div>
                {/if}
              </div>
            </section>
          {:else if settingsSection === 'updates'}
            <section class="settings-panel" aria-labelledby="updates-panel-title">
              <header class="settings-panel-heading">
                <h3 id="updates-panel-title">{$t('settings.updates.title')}</h3>
                <p>{$t('settings.updates.body')}</p>
              </header>
              <div class:available={!!updateCandidate} class:error={updateStatus === 'error'} class="update-status-panel">
                <span class:spinning={updateStatus === 'checking'}>
                  {#if updateCandidate}<Download size={22} />{:else if updateStatus === 'current'}<CheckCircle2 size={22} />{:else}<RefreshCw size={22} />{/if}
                </span>
                <div><strong>{updateCandidate ? $t('settings.updates.available', { version: updateCandidate.version }) : `msnnext ${appVersion}`}</strong><small>{updateMessage || $t('settings.updates.autoActive')}</small></div>
              </div>
              {#if updateStatus === 'downloading' || updateStatus === 'installing'}
                <div class="update-progress" aria-label={$t('settings.updates.progress', { percent: updateProgress })}><i style={`width: ${updateProgress}%`}></i></div>
              {/if}
              <div class="update-meta"><span>{$t('settings.updates.installed')}<strong>{appVersion}</strong></span><span>{$t('settings.updates.lastCheck')}<strong>{lastUpdateCheckLabel()}</strong></span></div>
              <div class="settings-action-row update-actions">
                <button class="secondary-button" disabled={updateStatus === 'checking' || updateStatus === 'downloading' || updateStatus === 'installing'} onclick={() => checkForUpdates(true)}><RefreshCw size={14} /> {$t('settings.updates.checkNow')}</button>
                {#if updateCandidate}<button class="primary-button" disabled={updateStatus === 'downloading' || updateStatus === 'installing'} onclick={installUpdate}><Download size={14} /> {updateStatus === 'downloading' ? $t('settings.updates.download', { percent: updateProgress || '…' }) : $t('settings.updates.downloadInstall')}</button>{/if}
              </div>
            </section>
          {:else}
            <section class="settings-panel" aria-labelledby="network-panel-title">
              <header class="settings-panel-heading">
                <h3 id="network-panel-title">{$t('settings.network.title')}</h3>
                <p>{$t('settings.network.body')}</p>
              </header>
              <label class="settings-field">{$t('settings.network.relay')}<input bind:value={relayAddress} maxlength="512" placeholder={$t('settings.network.relayPlaceholder')} /><small>{$t('settings.network.relayHint')}</small></label>
              <button class="security-settings-row" onclick={() => securityIntroOpen = true}><ShieldCheck size={19} /><span><strong>{$t('settings.network.security.title')}</strong><small>{$t('settings.network.security.desc')}</small></span><ExternalLink size={15} /></button>
            </section>
          {/if}
        </div>
      </div>

      <footer class="settings-footer">
        <small>{$t('settings.footer.note')}</small>
        <div><button class="secondary-button" onclick={() => profileOpen = false}>{$t('settings.footer.cancel')}</button><button class="primary-button" disabled={!displayName.trim()} onclick={() => saveProfile()}>{$t('settings.footer.save')}</button></div>
      </footer>
    </div>
  </div>
{/if}

{#if devicePairingOpen}
  <div class="modal-backdrop">
    <div class="modal device-pairing-modal" role="dialog" aria-modal="true" aria-labelledby="device-pairing-title">
      <button type="button" class="modal-close" aria-label={$t('window.close')} onclick={() => devicePairingOpen = false}><X size={18} /></button>
      <div class="modal-heading">
        <span>{#if devicePairingMode === 'share'}<QrCode size={22} />{:else}<Link2 size={22} />{/if}</span>
        <div><p class="step-label">{$t('pairing.step')}</p><h2 id="device-pairing-title">{devicePairingMode === 'share' ? $t('pairing.shareTitle') : $t('pairing.joinTitle')}</h2></div>
      </div>
      {#if devicePairingMode === 'share'}
        <p>{$t('pairing.shareBody')}</p>
        {#if devicePairingQr}
          <button class="device-pairing-qr" aria-label={$t('pairing.saveQrLabel')} title={$t('pairing.saveQr')} onclick={saveDevicePairingQr}><img src={devicePairingQr} alt={$t('pairing.qrAlt')} /></button>
          <div class="device-pairing-actions">
            <button class="secondary-button" onclick={copyDevicePairingLink}><Copy size={14} /> {$t('pairing.copyCode')}</button>
            <button class="secondary-button" onclick={saveDevicePairingQr}><Download size={14} /> {$t('pairing.saveQr')}</button>
          </div>
          <small>{$t('pairing.expires', { time: new Date(devicePairingExpiresAt).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) })}</small>
        {:else}
          <div class="device-pairing-loading">{$t('pairing.preparing')}</div>
        {/if}
      {:else}
        <p>{$t('pairing.joinBody')}</p>
        <label>{$t('pairing.deviceCode')}<textarea bind:value={devicePairingLink} rows="4" spellcheck="false" placeholder="msnnext://device/…"></textarea></label>
        <div class="device-pairing-actions">
          <button class="secondary-button" disabled={devicePairingBusy} onclick={scanDevicePairingQr}><QrCode size={14} /> {$t('pairing.openQr')}</button>
          <button class="primary-button" disabled={!devicePairingLink.trim() || devicePairingBusy} onclick={importDevicePairing}>{devicePairingBusy ? $t('pairing.linking') : $t('pairing.link')}</button>
        </div>
      {/if}
    </div>
  </div>
{/if}

{#if accountBackupOpen}
  <div class="modal-backdrop">
    <div class="modal account-backup-modal" role="dialog" aria-modal="true" aria-labelledby="account-backup-title">
      <button type="button" class="modal-close" aria-label={$t('window.close')} onclick={() => accountBackupOpen = false}><X size={18} /></button>
      <form onsubmit={(event) => { event.preventDefault(); void submitAccountBackup() }}>
        <div class="modal-heading">
          <span>{#if accountBackupMode === 'export'}<Download size={22} />{:else}<Upload size={22} />{/if}</span>
          <div><p class="step-label">{$t('backup.step')}</p><h2 id="account-backup-title">{accountBackupMode === 'export' ? $t('backup.exportTitle') : $t('backup.importTitle')}</h2></div>
        </div>
        <p>{accountBackupMode === 'export' ? $t('backup.exportBody') : $t('backup.importBody')}</p>
        <label>{$t('backup.password')}<input type="password" bind:value={accountBackupPassword} minlength="12" autocomplete={accountBackupMode === 'export' ? 'new-password' : 'current-password'} /></label>
        <small>{$t('backup.passwordHint')}</small>
        <button class="primary-button wide" disabled={accountBackupPassword.length < 12 || accountBackupBusy}>
          {accountBackupBusy ? $t('backup.wait') : accountBackupMode === 'export' ? $t('backup.createEncrypted') : $t('backup.restore')}
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
        <div><p class="step-label">{$t('security.step')}</p><h2 id="security-intro-title">{$t('security.title')}</h2></div>
      </div>
      <p class="security-intro-lead">{$t('security.lead')}</p>
      <ul class="security-intro-list">
        <li><LockKeyhole size={18} /><span><strong>{$t('security.item1.title')}</strong><small>{$t('security.item1.desc')}</small></span></li>
        <li><Sparkles size={18} /><span><strong>{$t('security.item2.title')}</strong><small>{$t('security.item2.desc')}</small></span></li>
        <li><ShieldCheck size={18} /><span><strong>{$t('security.item3.title')}</strong><small>{$t('security.item3.desc')}</small></span></li>
      </ul>
      <div class="security-caveat"><Info size={16} /><p><strong>{$t('security.caveatTitle')}</strong> {$t('security.caveatBody')}</p></div>
      <button class="primary-button wide" onclick={closeSecurityIntro}>{$t('security.gotIt')}</button>
    </div>
  </div>
{/if}

{#if messageMenu}
  {@const menu = messageMenu}
  <button class="context-scrim" aria-label={$t('ctx.close')} onclick={closeMessageMenu}></button>
  <div class="contact-context-menu" style={`left:${menu.x}px;top:${menu.y}px`} role="menu">
    <button onclick={() => deleteMessageForMe(menu.message)}>{$t('msg.deleteForMe')}</button>
    {#if canDeleteForEveryone(menu.message)}
      <button class="danger-item" onclick={() => deleteMessageForEveryone(menu.message)}>{$t('msg.deleteForEveryone')}</button>
    {/if}
  </div>
{/if}

{#if contactRequests.length}
  <div class="contact-requests" role="region" aria-label={$t('contactRequest.title')}>
    {#each contactRequests as request (request.peerId)}
      <div class="contact-request-card">
        <span class="avatar-shell contact-avatar"><span>{request.name.slice(0, 1).toUpperCase()}</span></span>
        <div class="contact-request-copy">
          <strong>{$t('contactRequest.title')}</strong>
          <small>{$t('contactRequest.wants', { name: request.name })}</small>
        </div>
        <div class="contact-request-actions">
          <button class="secondary-button" onclick={() => rejectContactRequest(request.peerId)}>{$t('offer.decline')}</button>
          <button class="primary-button" onclick={() => acceptContactRequest(request.peerId)}>{$t('offer.accept')}</button>
        </div>
      </div>
    {/each}
  </div>
{/if}

{#if toastText}
  <div class="toast" role="status"><MessageCircleMore size={16} />{toastText}</div>
{/if}
