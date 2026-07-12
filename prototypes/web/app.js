const $ = (id) => document.getElementById(id);
const els = Object.fromEntries([
  'appShell','connectButton','welcomeConnect','connectDialog','createCodeButton','pasteCodeButton','useCodeButton','connectionCode','connectHelp','nameInput',
  'peerName','peerStatus','peerPresence','peerCount','chatTitle','connectionLabel','nudgeButton','messages','welcome','composer','messageInput','sendButton',
  'fileButton','fileInput','emoteButton','emotePanel','emoteGrid','addEmoteButton','emoteDialog','emoteForm','emoteFile','emoteShortcut','toast','statusButton'
].map(id => [id,$(id)]));

const starterEmotes = [
  { shortcut: ':)', src: svg('☺','#0866ff') },
  { shortcut: ':D', src: svg('😁','#ffb020') },
  { shortcut: '<3', src: svg('♥','#f04468') },
  { shortcut: ':P', src: svg('😛','#7f56d9') },
  { shortcut: ';)', src: svg('😉','#12b76a') },
  { shortcut: ':(', src: svg('☹','#667085') }
];
let emotes = loadEmotes();
let pc, channel, localName = localStorage.getItem('nxt-name') || 'Amico';
const incomingFiles = new Map();

els.nameInput.value = localName;
renderEmotes();

els.connectButton.onclick = els.welcomeConnect.onclick = () => els.connectDialog.showModal();
els.createCodeButton.onclick = createOffer;
els.useCodeButton.onclick = useCode;
els.pasteCodeButton.onclick = async () => {
  try { els.connectionCode.value = await navigator.clipboard.readText(); }
  catch { toast('Incolla il codice nel campo qui sotto'); }
};
els.nudgeButton.onclick = () => { send({ type:'nudge' }); nudge('Hai inviato un trillo'); };
els.statusButton.onclick = () => toast('Sei disponibile');
els.fileButton.onclick = () => channel?.readyState === 'open' ? els.fileInput.click() : toast('Prima collega un amico');
els.fileInput.onchange = () => sendFile(els.fileInput.files[0]);
els.addEmoteButton.onclick = () => els.emoteDialog.showModal();
els.emoteButton.onclick = () => {
  if (matchMedia('(max-width:900px)').matches) els.emoteDialog.showModal();
  else els.emotePanel.classList.toggle('hidden');
};
els.emoteForm.onsubmit = async (event) => {
  event.preventDefault();
  const file = els.emoteFile.files[0], shortcut = els.emoteShortcut.value.trim();
  if (!file || !shortcut) return;
  if (file.size > 350_000) return toast('Per ora le emoticon possono pesare al massimo 350 KB');
  const emote = { shortcut, src: await readDataUrl(file) };
  saveEmote(emote);
  send({ type:'emote', emote });
  els.emoteForm.reset(); els.emoteDialog.close(); toast(`${shortcut} è pronta`);
};
els.composer.onsubmit = (event) => {
  event.preventDefault();
  const text = els.messageInput.value.trim();
  if (!text || !send({ type:'message', text })) return;
  addMessage(text, true); els.messageInput.value = ''; resizeInput();
};
els.messageInput.oninput = resizeInput;
els.messageInput.onkeydown = (event) => {
  if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); els.composer.requestSubmit(); }
};

function svg(face, color) {
  return `data:image/svg+xml,${encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="20" fill="${color}"/><text x="32" y="44" text-anchor="middle" font-size="38" fill="white">${face}</text></svg>`)}`;
}

function loadEmotes() {
  try { return JSON.parse(localStorage.getItem('nxt-emotes')) || starterEmotes; }
  catch { return starterEmotes; }
}

function saveEmote(emote) {
  emotes = [emote, ...emotes.filter(item => item.shortcut !== emote.shortcut)].slice(0,30);
  try { localStorage.setItem('nxt-emotes', JSON.stringify(emotes)); }
  catch { toast('Spazio locale esaurito: emoticon usabile solo ora'); }
  renderEmotes();
}

function renderEmotes() {
  els.emoteGrid.replaceChildren(...emotes.map(emote => {
    const button = document.createElement('button'); button.className = 'emote-item'; button.title = `Inserisci ${emote.shortcut}`;
    const image = new Image(); image.src = emote.src; image.alt = emote.shortcut;
    const label = document.createElement('small'); label.textContent = emote.shortcut;
    button.append(image,label); button.onclick = () => { els.messageInput.value += emote.shortcut; els.messageInput.focus(); };
    return button;
  }));
}

function newPeer() {
  pc?.close();
  pc = new RTCPeerConnection({ iceServers: [] });
  pc.ondatachannel = event => setupChannel(event.channel);
  pc.onconnectionstatechange = () => {
    if (['failed','disconnected','closed'].includes(pc.connectionState)) setConnected(false);
  };
  return pc;
}

async function createOffer() {
  localName = els.nameInput.value.trim() || 'Amico'; localStorage.setItem('nxt-name',localName);
  newPeer(); setupChannel(pc.createDataChannel('nxt'));
  await pc.setLocalDescription(await pc.createOffer());
  await iceComplete(pc);
  els.connectionCode.value = encode(pc.localDescription);
  els.connectionCode.select(); els.connectHelp.textContent = 'Invia questo codice all’altra persona e incolla qui la sua risposta.';
}

async function useCode() {
  try {
    localName = els.nameInput.value.trim() || 'Amico'; localStorage.setItem('nxt-name',localName);
    const description = decode(els.connectionCode.value.trim());
    if (!pc || pc.signalingState === 'closed') newPeer();
    if (description.type === 'offer') {
      newPeer(); await pc.setRemoteDescription(description); await pc.setLocalDescription(await pc.createAnswer()); await iceComplete(pc);
      els.connectionCode.value = encode(pc.localDescription); els.connectionCode.select();
      els.connectHelp.textContent = 'Risposta pronta: rimandala a chi ha creato il primo codice.';
    } else {
      await pc.setRemoteDescription(description); els.connectHelp.textContent = 'Collegamento in corso…';
    }
  } catch (error) { console.error(error); toast('Codice non valido o già usato'); }
}

function setupChannel(next) {
  channel = next;
  channel.onopen = () => { setConnected(true); send({ type:'hello', name:localName }); };
  channel.onclose = () => setConnected(false);
  channel.onmessage = event => receive(JSON.parse(event.data));
}

function send(payload) {
  if (channel?.readyState !== 'open') { toast('Prima collega un amico'); return false; }
  channel.send(JSON.stringify(payload)); return true;
}

function receive(data) {
  if (data.type === 'hello') {
    els.peerName.textContent = data.name || 'Amico'; els.chatTitle.textContent = data.name || 'Amico';
  } else if (data.type === 'message') addMessage(data.text, false);
  else if (data.type === 'nudge') nudge(`${els.peerName.textContent} ti ha inviato un trillo`);
  else if (data.type === 'emote' && data.emote?.shortcut && data.emote?.src?.startsWith('data:image/')) { saveEmote(data.emote); toast(`Nuova emoticon: ${data.emote.shortcut}`); }
  else if (data.type === 'file-start') incomingFiles.set(data.id,{ ...data, chunks:[] });
  else if (data.type === 'file-chunk') incomingFiles.get(data.id)?.chunks.push(data.chunk);
  else if (data.type === 'file-end') finishFile(data.id);
}

function setConnected(connected) {
  els.peerPresence.classList.toggle('online',connected); els.peerCount.textContent = connected ? '1 online' : '0 online';
  els.peerStatus.textContent = connected ? 'Connessione diretta' : 'Crea o inserisci un codice';
  els.connectionLabel.textContent = connected ? 'Online · peer-to-peer' : 'Non collegato';
  els.nudgeButton.disabled = els.messageInput.disabled = els.sendButton.disabled = !connected;
  if (connected) { els.connectDialog.close(); els.welcome?.remove(); addSystem('Connessione diretta stabilita'); els.messageInput.focus(); }
}

function addMessage(text, mine) {
  const row = document.createElement('div'); row.className = `message-row${mine ? ' mine':''}`;
  const avatar = document.createElement('div'); avatar.className='avatar'; avatar.textContent = mine ? 'T' : (els.peerName.textContent[0] || '?').toUpperCase();
  const bubble = document.createElement('div'); bubble.className='bubble';
  bubble.append(...tokenize(text).map(part => part.src ? Object.assign(new Image(),{ src:part.src, alt:part.text, className:'emote-inline' }) : document.createTextNode(part.text)));
  row.append(avatar,bubble); els.messages.append(row); scrollMessages();
}

function tokenize(text) {
  const shortcuts = [...emotes].sort((a,b) => b.shortcut.length-a.shortcut.length);
  const parts=[]; let rest=text;
  while (rest) {
    let index=rest.length, found=null;
    for (const emote of shortcuts) { const at=rest.indexOf(emote.shortcut); if (at>=0 && at<index) { index=at; found=emote; } }
    if (!found) { parts.push({text:rest}); break; }
    if (index) parts.push({text:rest.slice(0,index)});
    parts.push({text:found.shortcut,src:found.src}); rest=rest.slice(index+found.shortcut.length);
  }
  return parts;
}

async function sendFile(file) {
  els.fileInput.value='';
  if (!file) return;
  if (file.size > 15_000_000) return toast('Il limite del prototipo è 15 MB');
  const data = await readDataUrl(file), id = crypto.randomUUID(), size = 12_000;
  send({ type:'file-start', id, name:file.name, mime:file.type });
  for (let i=0;i<data.length;i+=size) { send({ type:'file-chunk', id, chunk:data.slice(i,i+size) }); if (channel.bufferedAmount > 500_000) await bufferDrain(); }
  send({ type:'file-end', id }); addAttachment(data,file.type,true); toast('File inviato');
}

function finishFile(id) {
  const file=incomingFiles.get(id); if (!file) return;
  addAttachment(file.chunks.join(''),file.mime,false); incomingFiles.delete(id); toast(`${file.name} ricevuto`);
}

function addAttachment(src,mime,mine) {
  const row=document.createElement('div'); row.className=`message-row${mine?' mine':''}`;
  const media=document.createElement(mime.startsWith('video/')?'video':'img'); media.src=src; media.controls=mime.startsWith('video/'); media.alt='Allegato condiviso';
  const box=document.createElement('div'); box.className='attachment'; box.append(media); row.append(box); els.messages.append(row); scrollMessages();
}

function nudge(label) {
  els.appShell.classList.remove('shake'); void els.appShell.offsetWidth; els.appShell.classList.add('shake'); addSystem(`〰 ${label}`);
  if (navigator.vibrate) navigator.vibrate([80,40,80]);
}
function addSystem(text) { const node=document.createElement('div'); node.className='system-message'; node.textContent=text; els.messages.append(node); scrollMessages(); }
function scrollMessages() { els.messages.scrollTop=els.messages.scrollHeight; }
function resizeInput() { els.messageInput.style.height='auto'; els.messageInput.style.height=`${els.messageInput.scrollHeight}px`; }
function readDataUrl(file) { return new Promise((resolve,reject) => { const reader=new FileReader(); reader.onload=()=>resolve(reader.result); reader.onerror=reject; reader.readAsDataURL(file); }); }
function bufferDrain() { return new Promise(resolve => { channel.bufferedAmountLowThreshold=100_000; channel.onbufferedamountlow=()=>{ channel.onbufferedamountlow=null; resolve(); }; }); }
function iceComplete(peer) { if (peer.iceGatheringState==='complete') return Promise.resolve(); return new Promise(resolve => peer.addEventListener('icegatheringstatechange',()=>peer.iceGatheringState==='complete'&&resolve())); }
function encode(value) { const bytes=new TextEncoder().encode(JSON.stringify(value)); let raw=''; bytes.forEach(byte=>raw+=String.fromCharCode(byte)); return btoa(raw); }
function decode(value) { const raw=atob(value); return JSON.parse(new TextDecoder().decode(Uint8Array.from(raw,char=>char.charCodeAt(0)))); }
function toast(message) { els.toast.textContent=message; els.toast.classList.add('show'); clearTimeout(toast.timer); toast.timer=setTimeout(()=>els.toast.classList.remove('show'),2600); }

// ponytail: browser self-check; move to a test runner when this grows beyond one module.
console.assert(tokenize('ciao :)!',starterEmotes).map(x=>x.text).join('')==='ciao :)!','shortcut tokenizer');
