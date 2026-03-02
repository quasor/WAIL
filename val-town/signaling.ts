// WAIL HTTP Signaling Server for Val Town
// Deploy: paste into a Val Town HTTP val at https://wail.val.run/
//
// Replaces the WebSocket signaling server with HTTP polling.
// Signaling is only used during connection setup (~12-22 messages).
// Once WebRTC DataChannels open, this server is never contacted again.

import { sqlite } from "https://esm.town/v/std/sqlite";

// ---------------------------------------------------------------------------
// Schema (idempotent)
// ---------------------------------------------------------------------------

await sqlite.batch([
  `CREATE TABLE IF NOT EXISTS peers (
    room TEXT NOT NULL,
    peer_id TEXT NOT NULL,
    last_seen INTEGER NOT NULL,
    PRIMARY KEY (room, peer_id)
  )`,
  `CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    room TEXT NOT NULL,
    to_peer TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
  )`,
  `CREATE INDEX IF NOT EXISTS idx_messages_dest
    ON messages (room, to_peer, id)`,
  `CREATE TABLE IF NOT EXISTS rooms (
    room TEXT PRIMARY KEY,
    password_hash TEXT NOT NULL
  )`,
]);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function now(): number {
  return Math.floor(Date.now() / 1000);
}

async function hashPassword(password: string): Promise<string> {
  const data = new TextEncoder().encode(password);
  const hash = await crypto.subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(hash))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

function json(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: {
      "Content-Type": "application/json",
      "Access-Control-Allow-Origin": "*",
    },
  });
}

async function enqueueForRoom(
  room: string,
  excludePeer: string,
  body: unknown,
): Promise<void> {
  const rows = await sqlite.execute({
    sql: "SELECT peer_id FROM peers WHERE room = ? AND peer_id != ?",
    args: [room, excludePeer],
  });
  const ts = now();
  const bodyStr = JSON.stringify(body);
  for (const row of rows.rows) {
    await sqlite.execute({
      sql: "INSERT INTO messages (room, to_peer, body, created_at) VALUES (?, ?, ?, ?)",
      args: [room, row[0] as string, bodyStr, ts],
    });
  }
}

async function cleanStalePeers(room: string): Promise<void> {
  const cutoff = now() - 30;
  const stale = await sqlite.execute({
    sql: "SELECT peer_id FROM peers WHERE room = ? AND last_seen < ?",
    args: [room, cutoff],
  });
  for (const row of stale.rows) {
    const stalePeer = row[0] as string;
    await sqlite.execute({
      sql: "DELETE FROM peers WHERE room = ? AND peer_id = ?",
      args: [room, stalePeer],
    });
    // Notify remaining peers
    await enqueueForRoom(room, stalePeer, {
      type: "PeerLeft",
      peer_id: stalePeer,
    });
  }
  // If no peers remain in the room, delete the room password so it can be re-created
  const remaining = await sqlite.execute({
    sql: "SELECT COUNT(*) FROM peers WHERE room = ?",
    args: [room],
  });
  if ((remaining.rows[0][0] as number) === 0) {
    await sqlite.execute({
      sql: "DELETE FROM rooms WHERE room = ?",
      args: [room],
    });
  }

  // Clean old messages (>60s)
  const msgCutoff = now() - 60;
  await sqlite.execute({
    sql: "DELETE FROM messages WHERE created_at < ?",
    args: [msgCutoff],
  });
}

// ---------------------------------------------------------------------------
// Listener HTML (embedded from web/listener.html)
// ---------------------------------------------------------------------------

function html(body: string, status = 200): Response {
  return new Response(body, {
    status,
    headers: { "Content-Type": "text/html; charset=utf-8" },
  });
}

const LISTENER_HTML = `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no">
  <title>WAIL Listener</title>
  <style>
    :root {
      --bg: #1a1a2e;
      --bg-card: #16213e;
      --fg: #e0e0e0;
      --fg-dim: #8888a0;
      --accent: #0f3460;
      --accent-bright: #4e9af1;
      --error: #e74c3c;
      --success: #2ecc71;
      --border: #2a2a4a;
      --radius: 6px;
    }
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      font-family: 'SF Mono', 'Menlo', 'Monaco', 'Consolas', monospace;
      font-size: 13px;
      background: var(--bg);
      color: var(--fg);
      padding: 24px 16px;
      min-height: 100vh;
      display: flex;
      justify-content: center;
    }
    #app { width: 100%; max-width: 420px; }
    h1 { font-size: 28px; font-weight: 700; letter-spacing: 4px; margin-bottom: 4px; }
    h2 { font-size: 18px; font-weight: 700; letter-spacing: 2px; }
    .subtitle { color: var(--fg-dim); font-size: 11px; margin-bottom: 24px; }
    label {
      display: block; font-size: 11px; color: var(--fg-dim);
      margin-top: 12px; margin-bottom: 4px;
      text-transform: uppercase; letter-spacing: 1px;
    }
    input[type="text"], input[type="password"] {
      width: 100%; padding: 10px; background: var(--bg-card);
      border: 1px solid var(--border); border-radius: var(--radius);
      color: var(--fg); font-family: inherit; font-size: 14px; outline: none;
    }
    input:focus { border-color: var(--accent-bright); }
    input[type="range"] { width: 100%; accent-color: var(--accent-bright); }
    button {
      width: 100%; padding: 12px; margin-top: 16px;
      background: var(--accent); border: 1px solid var(--accent-bright);
      border-radius: var(--radius); color: var(--fg); font-family: inherit;
      font-size: 13px; font-weight: 600; cursor: pointer; letter-spacing: 1px;
      min-height: 44px;
    }
    button:hover:not(:disabled) { background: var(--accent-bright); color: #fff; }
    button:disabled { opacity: 0.5; cursor: not-allowed; }
    details { margin-top: 12px; }
    summary {
      cursor: pointer; color: var(--fg-dim); font-size: 11px;
      text-transform: uppercase; letter-spacing: 1px;
    }
    .advanced-fields { padding-top: 4px; }
    .error {
      margin-top: 12px; padding: 8px 10px;
      background: rgba(231,76,60,0.15); border: 1px solid var(--error);
      border-radius: var(--radius); color: var(--error); font-size: 12px;
    }
    .session-header { display: flex; align-items: center; gap: 12px; margin-bottom: 20px; }
    .room-badge {
      background: var(--accent); border: 1px solid var(--accent-bright);
      border-radius: var(--radius); padding: 2px 10px; font-size: 12px;
    }
    .stat-group {
      margin-bottom: 12px; padding: 10px;
      background: var(--bg-card); border: 1px solid var(--border);
      border-radius: var(--radius);
    }
    .stat-group label { margin-top: 0; margin-bottom: 4px; }
    .stat-value { font-size: 16px; font-weight: 600; }
    .peer-list { display: flex; flex-direction: column; gap: 4px; }
    .peer-list .empty { color: var(--fg-dim); font-size: 12px; }
    .peer-item {
      display: flex; justify-content: space-between; align-items: center;
      padding: 4px 8px; background: rgba(255,255,255,0.03); border-radius: 4px;
    }
    .peer-name { font-weight: 600; }
    .peer-rtt { color: var(--fg-dim); font-size: 11px; }
    #disconnect-btn {
      background: transparent; border-color: var(--error);
      color: var(--error); margin-top: 20px;
    }
    #disconnect-btn:hover { background: var(--error); color: #fff; }
    #log-toggle { margin-top: 16px; }
    .log-badge {
      background: var(--accent); border-radius: 8px;
      padding: 1px 6px; font-size: 10px; margin-left: 4px;
    }
    .log-list { max-height: 200px; overflow-y: auto; margin-top: 8px; }
    .log-entry {
      padding: 3px 8px; font-size: 11px;
      border-left: 2px solid var(--border); margin-bottom: 2px; word-break: break-word;
    }
    .log-entry.info { border-left-color: var(--accent-bright); color: var(--fg-dim); }
    .log-entry.warn { border-left-color: #b8860b; color: #d4a843; }
    .log-entry.error { border-left-color: var(--error); color: var(--error); }
    .log-time { color: var(--fg-dim); margin-right: 6px; }
    .volume-row { display: flex; align-items: center; gap: 8px; }
    .volume-row input { flex: 1; }
    .volume-value { font-size: 12px; color: var(--fg-dim); min-width: 32px; text-align: right; }
    [hidden] { display: none !important; }
  </style>
</head>
<body>
  <div id="app">
    <div id="join-screen">
      <h1>WAIL</h1>
      <p class="subtitle">listen to a session</p>
      <label for="room">Room</label>
      <input type="text" id="room" placeholder="room name" autocapitalize="off" autocorrect="off">
      <label for="password">Password</label>
      <input type="password" id="password" placeholder="room password">
      <label for="display-name">Display Name</label>
      <input type="text" id="display-name" placeholder="optional">
      <div id="join-error" class="error" hidden></div>
      <button id="listen-btn">LISTEN</button>
    </div>
    <div id="session-screen" hidden>
      <div class="session-header">
        <h2>WAIL</h2>
        <span class="room-badge" id="room-name"></span>
      </div>
      <div class="stat-group">
        <label>Tempo</label>
        <span class="stat-value" id="bpm-display">--</span> <span style="color:var(--fg-dim)">BPM</span>
      </div>
      <div class="stat-group">
        <label>Peers</label>
        <div class="peer-list" id="peer-list">
          <span class="empty">waiting for peers...</span>
        </div>
      </div>
      <div class="stat-group">
        <label>Volume</label>
        <div class="volume-row">
          <input type="range" id="volume" min="0" max="100" value="80">
          <span class="volume-value" id="volume-value">80%</span>
        </div>
      </div>
      <div class="stat-group">
        <label>Audio</label>
        <span id="audio-stats">0 intervals received</span>
      </div>
      <button id="disconnect-btn">DISCONNECT</button>
      <details id="log-toggle">
        <summary>Log <span class="log-badge" id="log-badge">0</span></summary>
        <div class="log-list" id="log-list"></div>
      </details>
    </div>
  </div>
<script>
'use strict';
function nowUs(){return Date.now()*1000}
function defaultSignalingUrl(){
  if(location.hostname.includes('val'))return location.origin+location.pathname;
  return'https://wail.val.run/';
}
var MAX_LOG=200,logEntries=[],logWarnings=0,logErrors=0;
function log(level,msg){
  var time=new Date().toLocaleTimeString();
  logEntries.push({level:level,msg:msg,time:time});
  if(logEntries.length>MAX_LOG)logEntries.shift();
  if(level==='warn')logWarnings++;
  if(level==='error')logErrors++;
  renderLog();
  if(level==='error')console.error('[WAIL]',msg);
  else if(level==='warn')console.warn('[WAIL]',msg);
  else console.log('[WAIL]',msg);
}
function renderLog(){
  var list=document.getElementById('log-list'),badge=document.getElementById('log-badge');
  if(!list)return;
  list.innerHTML=logEntries.map(function(e){
    return'<div class="log-entry '+e.level+'"><span class="log-time">'+e.time+'</span>'+esc(e.msg)+'</div>';
  }).join('');
  list.scrollTop=list.scrollHeight;
  badge.textContent=logEntries.length;
  badge.className='log-badge'+(logErrors?' has-errors':logWarnings?' has-warnings':'');
}
function esc(s){return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;')}

class WailSignaling{
  constructor(baseUrl,room,peerId,password){
    this.baseUrl=baseUrl.replace(/\\/+$/,'');this.room=room;this.peerId=peerId;
    this.password=password;this.lastSeq=0;this.pollTimer=null;this.pollInterval=5000;
    this.onMessage=null;this.stopped=false;
  }
  async join(){
    var res=await fetch(this.baseUrl+'?action=join',{method:'POST',
      headers:{'Content-Type':'application/json'},
      body:JSON.stringify({room:this.room,peer_id:this.peerId,password:this.password})});
    if(!res.ok){var d=await res.json().catch(function(){return{}});throw new Error(d.error||'Join failed ('+res.status+')');}
    return res.json();
  }
  startPolling(){this.stopped=false;this._poll();}
  stopPolling(){this.stopped=true;if(this.pollTimer){clearTimeout(this.pollTimer);this.pollTimer=null;}}
  async _poll(){
    if(this.stopped)return;
    try{
      var url=this.baseUrl+'?action=poll&room='+encodeURIComponent(this.room)+'&peer_id='+encodeURIComponent(this.peerId)+'&after='+this.lastSeq;
      var res=await fetch(url);
      if(res.status===429){this.pollInterval=Math.min(this.pollInterval*2,30000);log('warn','Rate limited, backing off to '+this.pollInterval+'ms');}
      else if(res.ok){this.pollInterval=5000;var data=await res.json();for(var m of(data.messages||[])){this.lastSeq=Math.max(this.lastSeq,m.seq);if(this.onMessage)this.onMessage(m.body);}}
    }catch(e){log('warn','Poll error: '+e.message);}
    if(!this.stopped){this.pollTimer=setTimeout(()=>this._poll(),this.pollInterval);}
  }
  async signal(msg){
    try{await fetch(this.baseUrl+'?action=signal',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(msg)});}
    catch(e){log('warn','Signal send error: '+e.message);}
  }
  leave(){
    this.stopPolling();
    var body=JSON.stringify({room:this.room,peer_id:this.peerId});
    navigator.sendBeacon(this.baseUrl+'?action=leave',new Blob([body],{type:'application/json'}));
  }
}

var ICE_SERVERS=[{urls:'stun:stun.l.google.com:19302'}];
class WailPeerManager{
  constructor(localPeerId,displayName,signaling){
    this.localPeerId=localPeerId;this.displayName=displayName;this.signaling=signaling;
    this.peers=new Map();this.onSyncMessage=null;this.onAudioData=null;
    this.onPeerConnected=null;this.onPeerDisconnected=null;
    signaling.onMessage=(msg)=>this._handleSignalingMessage(msg);
  }
  _handleSignalingMessage(msg){
    if(msg.type==='PeerJoined'){log('info','Peer joined: '+msg.peer_id);this._evaluateConnection(msg.peer_id);}
    else if(msg.type==='PeerLeft'){log('info','Peer left: '+msg.peer_id);this._removePeer(msg.peer_id);}
    else if(msg.type==='Signal'){this._handleSignal(msg.from,msg.payload);}
  }
  handleInitialPeers(peerIds){for(var pid of peerIds)this._evaluateConnection(pid);}
  _evaluateConnection(remotePeerId){
    if(this.peers.has(remotePeerId))return;
    if(this.localPeerId<remotePeerId)this._initiateConnection(remotePeerId);
  }
  async _initiateConnection(remotePeerId){
    log('info','Initiating connection to '+remotePeerId.slice(0,8)+'...');
    var peer=this._createPeer(remotePeerId);
    var dcSync=peer.pc.createDataChannel('sync');
    var dcAudio=peer.pc.createDataChannel('audio');dcAudio.binaryType='arraybuffer';
    peer.dcSync=dcSync;peer.dcAudio=dcAudio;
    this._setupSyncChannel(dcSync,remotePeerId);this._setupAudioChannel(dcAudio,remotePeerId);
    try{var offer=await peer.pc.createOffer();await peer.pc.setLocalDescription(offer);
      this.signaling.signal({type:'Signal',to:remotePeerId,from:this.localPeerId,payload:{kind:'Offer',sdp:offer.sdp}});
    }catch(e){log('error','Failed to create offer for '+remotePeerId.slice(0,8)+': '+e.message);}
  }
  async _handleSignal(fromId,payload){
    if(payload.kind==='Offer'){
      log('info','Received offer from '+fromId.slice(0,8)+'...');
      var peer=this.peers.get(fromId);if(!peer)peer=this._createPeer(fromId);
      peer.pc.ondatachannel=(event)=>{var dc=event.channel;
        if(dc.label==='sync'){peer.dcSync=dc;this._setupSyncChannel(dc,fromId);}
        else if(dc.label==='audio'){dc.binaryType='arraybuffer';peer.dcAudio=dc;this._setupAudioChannel(dc,fromId);}
      };
      try{await peer.pc.setRemoteDescription(new RTCSessionDescription({type:'offer',sdp:payload.sdp}));
        peer.remoteDescSet=true;for(var c of peer.pendingCandidates)await peer.pc.addIceCandidate(c);peer.pendingCandidates=[];
        var answer=await peer.pc.createAnswer();await peer.pc.setLocalDescription(answer);
        this.signaling.signal({type:'Signal',to:fromId,from:this.localPeerId,payload:{kind:'Answer',sdp:answer.sdp}});
      }catch(e){log('error','Failed to handle offer from '+fromId.slice(0,8)+': '+e.message);}
    }else if(payload.kind==='Answer'){
      var peer=this.peers.get(fromId);if(!peer){log('warn','Answer from unknown peer '+fromId.slice(0,8));return;}
      try{await peer.pc.setRemoteDescription(new RTCSessionDescription({type:'answer',sdp:payload.sdp}));
        peer.remoteDescSet=true;for(var c of peer.pendingCandidates)await peer.pc.addIceCandidate(c);peer.pendingCandidates=[];
      }catch(e){log('error','Failed to handle answer from '+fromId.slice(0,8)+': '+e.message);}
    }else if(payload.kind==='IceCandidate'){
      var peer=this.peers.get(fromId);if(!peer)peer=this._createPeer(fromId);
      var candidate=new RTCIceCandidate({candidate:payload.candidate,sdpMid:payload.sdp_mid,sdpMLineIndex:payload.sdp_mline_index});
      if(peer.remoteDescSet){try{await peer.pc.addIceCandidate(candidate);}catch(e){log('warn','Failed to add ICE candidate: '+e.message);}}
      else peer.pendingCandidates.push(candidate);
    }
  }
  _createPeer(remotePeerId){
    var pc=new RTCPeerConnection({iceServers:ICE_SERVERS});
    var peer={pc:pc,dcSync:null,dcAudio:null,displayName:null,remoteDescSet:false,pendingCandidates:[]};
    this.peers.set(remotePeerId,peer);
    pc.onicecandidate=(event)=>{if(event.candidate)this.signaling.signal({type:'Signal',to:remotePeerId,from:this.localPeerId,
      payload:{kind:'IceCandidate',candidate:event.candidate.candidate,sdp_mid:event.candidate.sdpMid,sdp_mline_index:event.candidate.sdpMLineIndex}});};
    pc.onconnectionstatechange=()=>{
      log('info','Peer '+remotePeerId.slice(0,8)+' connection: '+pc.connectionState);
      if(pc.connectionState==='connected'&&this.onPeerConnected)this.onPeerConnected(remotePeerId);
      if(pc.connectionState==='failed'||pc.connectionState==='disconnected')log('warn','Peer '+remotePeerId.slice(0,8)+' '+pc.connectionState);
    };
    return peer;
  }
  _setupSyncChannel(dc,remotePeerId){
    dc.onopen=()=>{log('info','Sync channel open with '+remotePeerId.slice(0,8));
      dc.send(JSON.stringify({type:'Hello',peer_id:this.localPeerId,display_name:this.displayName||null}));
      dc.send(JSON.stringify({type:'AudioCapabilities',sample_rates:[48000],channel_counts:[1,2],can_send:false,can_receive:true}));};
    dc.onmessage=(event)=>{try{var msg=JSON.parse(event.data);if(this.onSyncMessage)this.onSyncMessage(remotePeerId,msg);}
      catch(e){log('warn','Failed to parse sync message: '+e.message);}};
  }
  _setupAudioChannel(dc,remotePeerId){
    dc.binaryType='arraybuffer';
    dc.onopen=()=>{log('info','Audio channel open with '+remotePeerId.slice(0,8));};
    dc.onmessage=(event)=>{if(this.onAudioData)this.onAudioData(remotePeerId,event.data);};
  }
  sendSync(remotePeerId,msg){var peer=this.peers.get(remotePeerId);if(peer&&peer.dcSync&&peer.dcSync.readyState==='open')peer.dcSync.send(JSON.stringify(msg));}
  _removePeer(remotePeerId){var peer=this.peers.get(remotePeerId);if(peer){peer.pc.close();this.peers.delete(remotePeerId);if(this.onPeerDisconnected)this.onPeerDisconnected(remotePeerId);}}
  closeAll(){for(var[id,peer]of this.peers)peer.pc.close();this.peers.clear();}
}

var WACH_MAGIC=0x48434157,WAIL_HEADER_SIZE=48;
class WailWireParser{
  constructor(onInterval){this.onInterval=onInterval;this.reassembly=new Map();}
  handleAudioData(peerId,data){
    var bytes=new Uint8Array(data);
    if(bytes.length>=8){var view=new DataView(data);var magic=view.getUint32(0,true);
      if(magic===WACH_MAGIC){var totalLen=view.getUint32(4,true);var payload=bytes.slice(8);
        var state=this.reassembly.get(peerId);
        if(!state||state.expectedLen!==totalLen){state={chunks:[],receivedLen:0,expectedLen:totalLen};this.reassembly.set(peerId,state);}
        state.chunks.push(payload);state.receivedLen+=payload.length;
        if(state.receivedLen>=state.expectedLen){var complete=new Uint8Array(state.receivedLen);var offset=0;
          for(var chunk of state.chunks){complete.set(chunk,offset);offset+=chunk.length;}
          this.reassembly.delete(peerId);this._parseWire(peerId,complete.buffer);}
        return;}}
    this._parseWire(peerId,data);
  }
  _parseWire(peerId,data){
    var bytes=new Uint8Array(data);
    if(bytes.length<WAIL_HEADER_SIZE){log('warn','Wire data too short: '+bytes.length+' bytes');return;}
    if(bytes[0]!==0x57||bytes[1]!==0x41||bytes[2]!==0x49||bytes[3]!==0x4C){log('warn','Invalid wire magic');return;}
    var version=bytes[4];if(version!==1){log('warn','Unsupported wire version: '+version);return;}
    var view=new DataView(data);var flags=bytes[5];var channels=(flags&1)?2:1;
    var indexLow=view.getUint32(8,true);var indexHigh=view.getInt32(12,true);var index=indexHigh*0x100000000+indexLow;
    var sampleRate=view.getUint32(16,true);var numFrames=view.getUint32(20,true);
    var bpm=view.getFloat64(24,true);var quantum=view.getFloat64(32,true);
    var bars=view.getUint32(40,true);var opusDataLen=view.getUint32(44,true);
    if(bytes.length<WAIL_HEADER_SIZE+opusDataLen){log('warn','Wire data truncated: need '+opusDataLen+' opus bytes, have '+(bytes.length-WAIL_HEADER_SIZE));return;}
    var opusData=new Uint8Array(data,WAIL_HEADER_SIZE,opusDataLen);
    this.onInterval(peerId,{index:index,sampleRate:sampleRate,channels:channels,numFrames:numFrames,bpm:bpm,quantum:quantum,bars:bars,opusData:opusData});
  }
}

function makeOpusDescription(channels,sampleRate){
  var head=new Uint8Array(19);var view=new DataView(head.buffer);
  head.set([0x4F,0x70,0x75,0x73,0x48,0x65,0x61,0x64]);
  head[8]=1;head[9]=channels;view.setUint16(10,3840,true);view.setUint32(12,sampleRate,true);view.setUint16(16,0,true);head[18]=0;
  return head;
}
class WailAudioPlayer{
  constructor(){this.audioCtx=null;this.gainNode=null;this.peerPlayTimes=new Map();this.intervalsReceived=0;}
  init(){
    this.audioCtx=new AudioContext({sampleRate:48000});this.gainNode=this.audioCtx.createGain();
    this.gainNode.connect(this.audioCtx.destination);this.setVolume(80);
    log('info','AudioContext created (state: '+this.audioCtx.state+', rate: '+this.audioCtx.sampleRate+')');
  }
  setVolume(pct){if(this.gainNode)this.gainNode.gain.value=pct/100;}
  async resume(){if(this.audioCtx&&this.audioCtx.state==='suspended')await this.audioCtx.resume();}
  async decodeAndPlay(peerId,interval){
    if(!this.audioCtx)return;await this.resume();
    var opusData=interval.opusData,sampleRate=interval.sampleRate,channels=interval.channels;
    if(opusData.byteLength<4){log('warn','Opus data too short');return;}
    var opusView=new DataView(opusData.buffer,opusData.byteOffset,opusData.byteLength);
    var numOpusFrames=opusView.getUint32(0,true);var packets=[];var offset=4;
    for(var i=0;i<numOpusFrames;i++){if(offset+2>opusData.byteLength)break;var pktLen=opusView.getUint16(offset,true);offset+=2;
      if(offset+pktLen>opusData.byteLength)break;packets.push(opusData.slice(offset,offset+pktLen));offset+=pktLen;}
    if(packets.length===0)return;
    try{var channelBuffers=await this._decodePackets(packets,sampleRate,channels);
      if(channelBuffers&&channelBuffers[0].length>0){this._schedulePlayback(peerId,channelBuffers,sampleRate);this.intervalsReceived++;}
    }catch(e){log('error','Decode failed: '+e.message);}
  }
  _decodePackets(packets,sampleRate,channels){
    return new Promise(function(resolve,reject){
      var channelChunks=Array.from({length:channels},function(){return[];});var errored=false;
      var decoder=new AudioDecoder({
        output:function(audioData){try{for(var ch=0;ch<audioData.numberOfChannels;ch++){var buf=new Float32Array(audioData.numberOfFrames);
          audioData.copyTo(buf,{planeIndex:ch});if(ch<channels)channelChunks[ch].push(buf);}}finally{audioData.close();}},
        error:function(e){if(!errored){errored=true;reject(e);}}
      });
      decoder.configure({codec:'opus',sampleRate:sampleRate,numberOfChannels:channels,description:makeOpusDescription(channels,sampleRate)});
      var timestamp=0;for(var pkt of packets){decoder.decode(new EncodedAudioChunk({type:'key',timestamp:timestamp,data:pkt}));timestamp+=20000;}
      decoder.flush().then(function(){decoder.close();
        var result=channelChunks.map(function(chunks){var total=chunks.reduce(function(s,c){return s+c.length;},0);var buf=new Float32Array(total);var off=0;
          for(var chunk of chunks){buf.set(chunk,off);off+=chunk.length;}return buf;});
        resolve(result);}).catch(reject);
    });
  }
  _schedulePlayback(peerId,channelBuffers,sampleRate){
    var numChannels=channelBuffers.length;var numSamples=channelBuffers[0].length;if(numSamples===0)return;
    var audioBuffer=this.audioCtx.createBuffer(numChannels,numSamples,sampleRate);
    for(var ch=0;ch<numChannels;ch++)audioBuffer.getChannelData(ch).set(channelBuffers[ch]);
    var source=this.audioCtx.createBufferSource();source.buffer=audioBuffer;source.connect(this.gainNode);
    var now=this.audioCtx.currentTime;var playTime=this.peerPlayTimes.get(peerId)||0;
    if(playTime<=now)playTime=now+0.05;source.start(playTime);this.peerPlayTimes.set(peerId,playTime+audioBuffer.duration);
  }
  removePeer(peerId){this.peerPlayTimes.delete(peerId);}
  close(){if(this.audioCtx){this.audioCtx.close();this.audioCtx=null;}}
}

class WailSession{
  constructor(config){
    this.room=config.room;this.password=config.password;this.displayName=config.displayName;
    this.serverUrl=config.serverUrl||defaultSignalingUrl();this.peerId=crypto.randomUUID();
    this.signaling=null;this.peerManager=null;this.wireParser=null;this.audioPlayer=null;
    this.bpm=null;this.peerInfo=new Map();this.onUpdate=null;
  }
  async start(){
    this.audioPlayer=new WailAudioPlayer();this.audioPlayer.init();
    this.signaling=new WailSignaling(this.serverUrl,this.room,this.peerId,this.password);
    var self=this;
    this.wireParser=new WailWireParser(function(peerId,interval){
      if(self.bpm===null||Math.abs(self.bpm-interval.bpm)>0.01)self.bpm=interval.bpm;
      self.audioPlayer.decodeAndPlay(peerId,interval);if(self.onUpdate)self.onUpdate();
    });
    this.peerManager=new WailPeerManager(this.peerId,this.displayName,this.signaling);
    this.peerManager.onSyncMessage=function(peerId,msg){self._handleSync(peerId,msg);};
    this.peerManager.onAudioData=function(peerId,data){self.wireParser.handleAudioData(peerId,data);};
    this.peerManager.onPeerDisconnected=function(peerId){self.peerInfo.delete(peerId);self.audioPlayer.removePeer(peerId);if(self.onUpdate)self.onUpdate();};
    log('info','Joining room "'+this.room+'" as '+this.peerId.slice(0,8)+'...');
    var result=await this.signaling.join();var peers=result.peers;
    log('info','Joined room. '+peers.length+' peer(s) present.');
    this.signaling.startPolling();this.peerManager.handleInitialPeers(peers);
    window.addEventListener('beforeunload',function(){self.signaling.leave();});
    if(this.onUpdate)this.onUpdate();
  }
  _handleSync(peerId,msg){
    if(msg.type==='Hello'){var name=msg.display_name||msg.peer_id.slice(0,8);log('info','Hello from '+name);
      if(!this.peerInfo.has(peerId))this.peerInfo.set(peerId,{});this.peerInfo.get(peerId).displayName=name;if(this.onUpdate)this.onUpdate();}
    else if(msg.type==='Ping'){this.peerManager.sendSync(peerId,{type:'Pong',id:msg.id,ping_sent_at_us:msg.sent_at_us,pong_sent_at_us:nowUs()});}
    else if(msg.type==='Pong'){var rtt=(nowUs()-msg.ping_sent_at_us)/1000;
      if(!this.peerInfo.has(peerId))this.peerInfo.set(peerId,{});this.peerInfo.get(peerId).rtt=Math.round(rtt);if(this.onUpdate)this.onUpdate();}
    else if(msg.type==='TempoChange'||msg.type==='StateSnapshot'){this.bpm=msg.bpm;if(this.onUpdate)this.onUpdate();}
  }
  setVolume(pct){if(this.audioPlayer)this.audioPlayer.setVolume(pct);}
  stop(){if(this.signaling)this.signaling.leave();if(this.peerManager)this.peerManager.closeAll();if(this.audioPlayer)this.audioPlayer.close();this.peerInfo.clear();}
}

var session=null;
function showScreen(id){document.getElementById('join-screen').hidden=(id!=='join');document.getElementById('session-screen').hidden=(id!=='session');}
function updateSessionUI(){
  if(!session)return;
  document.getElementById('bpm-display').textContent=session.bpm!==null?session.bpm.toFixed(1):'--';
  document.getElementById('room-name').textContent=session.room;
  var listEl=document.getElementById('peer-list');
  if(session.peerInfo.size===0){listEl.innerHTML='<span class="empty">waiting for peers...</span>';}
  else{listEl.innerHTML='';for(var[pid,info]of session.peerInfo){var div=document.createElement('div');div.className='peer-item';
    var name=info.displayName||pid.slice(0,8);var rtt=info.rtt!=null?info.rtt+'ms':'...';
    div.innerHTML='<span class="peer-name">'+esc(name)+'</span><span class="peer-rtt">'+rtt+'</span>';listEl.appendChild(div);}}
  var count=session.audioPlayer?session.audioPlayer.intervalsReceived:0;
  document.getElementById('audio-stats').textContent=count+' interval'+(count!==1?'s':'')+' received';
}
document.getElementById('listen-btn').addEventListener('click',async function(){
  var room=document.getElementById('room').value.trim();var password=document.getElementById('password').value;
  var displayName=document.getElementById('display-name').value.trim();
  var errorEl=document.getElementById('join-error');errorEl.hidden=true;
  if(!room){errorEl.textContent='Room name is required';errorEl.hidden=false;return;}
  if(!password){errorEl.textContent='Password is required';errorEl.hidden=false;return;}
  if(typeof AudioDecoder==='undefined'){errorEl.textContent='Your browser does not support audio decoding (WebCodecs). Use Chrome, Safari, or Firefox.';errorEl.hidden=false;return;}
  var btn=document.getElementById('listen-btn');btn.disabled=true;btn.textContent='CONNECTING...';
  try{session=new WailSession({room:room,password:password,displayName:displayName});
    session.onUpdate=updateSessionUI;await session.start();showScreen('session');updateSessionUI();
  }catch(e){errorEl.textContent=e.message;errorEl.hidden=false;if(session){session.stop();session=null;}}
  finally{btn.disabled=false;btn.textContent='LISTEN';}
  try{localStorage.setItem('wail-listener',JSON.stringify({room:room,displayName:displayName}));}catch(x){}
});
document.getElementById('disconnect-btn').addEventListener('click',function(){
  if(session){session.stop();session=null;}showScreen('join');logEntries.length=0;logWarnings=0;logErrors=0;renderLog();
});
document.getElementById('volume').addEventListener('input',function(e){
  var pct=parseInt(e.target.value,10);document.getElementById('volume-value').textContent=pct+'%';if(session)session.setVolume(pct);
});
try{var saved=JSON.parse(localStorage.getItem('wail-listener')||'{}');
  if(saved.room)document.getElementById('room').value=saved.room;
  if(saved.displayName)document.getElementById('display-name').value=saved.displayName;
}catch(x){}
</script>
</body>
</html>`;

// ---------------------------------------------------------------------------
// Request handler
// ---------------------------------------------------------------------------

export default async function(req: Request): Promise<Response> {
  // CORS preflight
  if (req.method === "OPTIONS") {
    return new Response(null, {
      headers: {
        "Access-Control-Allow-Origin": "*",
        "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
        "Access-Control-Allow-Headers": "Content-Type",
      },
    });
  }

  const url = new URL(req.url);
  const action = url.searchParams.get("action");

  try {
    switch (action) {
      // -------------------------------------------------------------------
      // POST ?action=join  body: { room, peer_id, password }
      // -------------------------------------------------------------------
      case "join": {
        const { room, peer_id, password } = await req.json();
        if (!room || !peer_id) return json({ error: "room and peer_id required" }, 400);
        if (!password) return json({ error: "password required" }, 400);

        // Check or create room password
        const pwHash = await hashPassword(password);
        const roomRow = await sqlite.execute({
          sql: "SELECT password_hash FROM rooms WHERE room = ?",
          args: [room],
        });
        if (roomRow.rows.length === 0) {
          // First peer creates the room
          await sqlite.execute({
            sql: "INSERT INTO rooms (room, password_hash) VALUES (?, ?)",
            args: [room, pwHash],
          });
        } else {
          const storedHash = roomRow.rows[0][0] as string;
          if (storedHash !== pwHash) {
            return json({ error: "invalid password" }, 401);
          }
        }

        const ts = now();

        // Upsert peer
        await sqlite.execute({
          sql: `INSERT INTO peers (room, peer_id, last_seen) VALUES (?, ?, ?)
                ON CONFLICT (room, peer_id) DO UPDATE SET last_seen = ?`,
          args: [room, peer_id, ts, ts],
        });

        // Get current peers (excluding self)
        const rows = await sqlite.execute({
          sql: "SELECT peer_id FROM peers WHERE room = ? AND peer_id != ?",
          args: [room, peer_id],
        });
        const peers = rows.rows.map((r) => r[0] as string);

        // Enqueue PeerJoined for existing peers
        await enqueueForRoom(room, peer_id, {
          type: "PeerJoined",
          peer_id,
        });

        return json({ peers });
      }

      // -------------------------------------------------------------------
      // POST ?action=leave  body: { room, peer_id }
      // -------------------------------------------------------------------
      case "leave": {
        const { room, peer_id } = await req.json();
        if (!room || !peer_id) return json({ error: "room and peer_id required" }, 400);

        await sqlite.execute({
          sql: "DELETE FROM peers WHERE room = ? AND peer_id = ?",
          args: [room, peer_id],
        });

        // Enqueue PeerLeft for remaining peers
        await enqueueForRoom(room, peer_id, {
          type: "PeerLeft",
          peer_id,
        });

        // Clean up messages for this peer
        await sqlite.execute({
          sql: "DELETE FROM messages WHERE room = ? AND to_peer = ?",
          args: [room, peer_id],
        });

        return json({ ok: true });
      }

      // -------------------------------------------------------------------
      // POST ?action=signal  body: SignalMessage (has to, from, payload)
      // -------------------------------------------------------------------
      case "signal": {
        const body = await req.json();
        if (!body.to || !body.from) return json({ error: "to and from required" }, 400);

        // Find the room for the sender
        const row = await sqlite.execute({
          sql: "SELECT room FROM peers WHERE peer_id = ? LIMIT 1",
          args: [body.from],
        });
        if (row.rows.length === 0) return json({ error: "sender not in any room" }, 404);

        const room = row.rows[0][0] as string;
        const ts = now();

        await sqlite.execute({
          sql: "INSERT INTO messages (room, to_peer, body, created_at) VALUES (?, ?, ?, ?)",
          args: [room, body.to, JSON.stringify(body), ts],
        });

        return json({ ok: true });
      }

      // -------------------------------------------------------------------
      // GET ?action=poll&room=X&peer_id=Y&after=SEQ
      // -------------------------------------------------------------------
      case "poll": {
        const room = url.searchParams.get("room");
        const peer_id = url.searchParams.get("peer_id");
        const after = parseInt(url.searchParams.get("after") || "0", 10);

        if (!room || !peer_id) return json({ error: "room and peer_id required" }, 400);

        // Heartbeat: update last_seen
        await sqlite.execute({
          sql: "UPDATE peers SET last_seen = ? WHERE room = ? AND peer_id = ?",
          args: [now(), room, peer_id],
        });

        // Clean stale peers
        await cleanStalePeers(room);

        // Fetch queued messages
        const rows = await sqlite.execute({
          sql: "SELECT id, body FROM messages WHERE room = ? AND to_peer = ? AND id > ? ORDER BY id ASC LIMIT 50",
          args: [room, peer_id, after],
        });

        const messages = rows.rows.map((r) => ({
          seq: r[0] as number,
          body: JSON.parse(r[1] as string),
        }));

        return json({ messages });
      }

      case null:
        return html(LISTENER_HTML);

      default:
        return json({ error: `unknown action: ${action}` }, 400);
    }
  } catch (e: any) {
    return json({ error: e.message }, 500);
  }
}
