// Independent Java 1.21.1 client; no account or existing configuration is read.
const assert = require('node:assert/strict')
const mc = require('minecraft-protocol')
const port = Number(process.env.ZCRAFT_TEST_PORT)
assert(Number.isInteger(port) && port > 0)
const clients = []
const deadline = setTimeout(() => {
  console.error('chat test deadline exceeded')
  for (const client of clients) client.end()
  process.exit(1)
}, 20000)

async function waitFor (predicate, description) {
  const end = Date.now() + 4000
  while (Date.now() < end) {
    if (predicate()) return
    await new Promise(resolve => setTimeout(resolve, 20))
  }
  throw new Error('timed out: ' + description)
}

async function join (username) {
  const client = mc.createClient({
    host: '127.0.0.1', port, username, auth: 'offline', version: '1.21.1',
    profilesFolder: false, disableChatSigning: true, keepAlive: true
  })
  clients.push(client)
  let positioned = false
  const state = { client, messages: [], heartbeats: 0, tree: null }
  client.on('error', error => {
    console.error(error)
    process.exitCode = 1
  })
  client.on('position', packet => {
    client.write('teleport_confirm', { teleportId: packet.teleportId })
    positioned = true
  })
  client.on('chunk_batch_finished', () => {
    client.write('chunk_batch_received', { chunksPerTick: 8 })
  })
  client.on('declare_commands', packet => { state.tree = packet })
  client.on('packet', (packet, meta, raw) => {
    if (meta.name !== 'system_chat') return
    assert.equal(packet.isActionBar, false)
    assert.equal(packet.content.type, 'string')
    // prismarine-nbt uses standard UTF-8 and replaces surrogate code units.
    // Inspect the decoded frame's raw TAG_String with Java's UTF-16 encoding.
    assert.equal(raw[0], 0x6c)
    assert.equal(raw[1], 8)
    const end = 4 + raw.readUInt16BE(2)
    assert.equal(raw.length, end + 1)
    let text = ''
    for (let i = 4; i < end;) {
      const first = raw[i++]
      if (first < 0x80) text += String.fromCharCode(first)
      else if (first < 0xe0) text += String.fromCharCode(((first & 31) << 6) | (raw[i++] & 63))
      else text += String.fromCharCode(((first & 15) << 12) | ((raw[i++] & 63) << 6) | (raw[i++] & 63))
    }
    state.messages.push(text)
  })
  client.on('player_chat', () => assert.fail('unexpected signed/player chat packet'))
  client.on('keep_alive', () => { state.heartbeats++ })
  await waitFor(() => positioned && state.tree, username + ' Play initialization')
  return state
}

function expectTree ({ nodes, rootIndex }) {
  assert.equal(rootIndex, 0)
  const root = nodes[rootIndex]
  assert.equal(root.flags.command_node_type, 0)
  const commands = Object.fromEntries(root.children.map(i => [nodes[i].extraNodeData.name, nodes[i]]))
  assert.deepEqual(Object.keys(commands).sort(), ['fixture', 'help', 'list', 'me', 'msg', 'tell'])
  for (const command of ['help', 'list']) assert.equal(commands[command].flags.has_command, 1)
  for (const command of ['msg', 'tell']) {
    const branch = command === 'tell' ? nodes[commands.tell.redirectNode] : commands.msg
    assert.equal(branch, commands.msg)
    const target = nodes[branch.children[0]]
    assert.equal(target.extraNodeData.name, 'target')
    assert.equal(target.extraNodeData.parser, 'brigadier:string')
    assert.equal(target.extraNodeData.properties, 'SINGLE_WORD')
    const message = nodes[target.children[0]]
    assert.equal(message.extraNodeData.name, 'message')
    assert.equal(message.extraNodeData.properties, 'GREEDY_PHRASE')
    assert.equal(message.flags.has_command, 1)
  }
  assert.equal(nodes[commands.me.children[0]].extraNodeData.properties, 'GREEDY_PHRASE')
  const fixture = nodes[commands.fixture.children[0]]
  assert.equal(fixture.flags.has_custom_suggestions, 1)
  assert.equal(fixture.extraNodeData.suggestionType, 'minecraft:ask_server')
  assert.equal(nodes[commands.me.children[0]].flags.has_custom_suggestions, 0)
}

function settings (state, chatFlags) {
  state.client.write('settings', {
    locale: 'en_us', viewDistance: 2, chatFlags, chatColors: true,
    skinParts: 127, mainHand: 1, enableTextFiltering: false, enableServerListing: true
  })
}

async function expectMessage (state, text) {
  try {
    await waitFor(() => state.messages.includes(text), JSON.stringify(text))
  } catch (error) {
    throw new Error(error.message + '; received: ' + JSON.stringify(state.messages))
  }
}

async function run () {
  const alice = await join('Alice')
  const bob = await join('Bob')
  const cara = await join('Cara')
  expectTree(alice.tree)
  async function complete (state, transactionId, text) {
    let reply
    const receive = packet => { if (packet.transactionId === transactionId) reply = packet }
    state.client.on('tab_complete', receive)
    try {
      state.client.write('tab_complete', { transactionId, text })
      await waitFor(() => reply, 'completion transaction ' + transactionId)
      return reply
    } finally {
      state.client.removeListener('tab_complete', receive)
    }
  }
  const literal = await complete(alice, 17, '/he')
  assert.equal(literal.start, 1)
  assert.equal(literal.length, 2)
  assert.deepEqual(literal.matches.map(value => value.match), ['help'])
  const unicode = await complete(bob, 18, '/fixture 🙂')
  assert.equal(unicode.start, 9)
  assert.equal(unicode.length, 2)
  assert.equal(unicode.matches[0].match, '你好🙂')
  assert.equal(unicode.matches[0].tooltip.value, '异步提示')
  assert.deepEqual((await complete(cara, 19, '/fixture_hidden')).matches, [])
  await new Promise(resolve => setTimeout(resolve, 230))
  assert.deepEqual((await complete(bob, 20, '/fixture wait')).matches, [])
  cara.client.chat('/list')
  await expectMessage(cara, 'Online (3): Alice, Bob, Cara')

  alice.client.chat('你好 🙂')
  await Promise.all([alice, bob, cara].map(state => expectMessage(state, '<Alice> 你好 🙂')))
  alice.client.chat('/msg bob secret 内容')
  await expectMessage(bob, '[From Alice] secret 内容')
  await expectMessage(alice, '[To Bob] secret 内容')
  bob.client.write('chat_command_signed', {
    command: 'me waves', timestamp: BigInt(Date.now()), salt: 1n,
    argumentSignatures: [{ argumentName: 'action', signature: Buffer.alloc(256, 7) }],
    messageCount: 0, acknowledged: Buffer.alloc(3)
  })
  await Promise.all([alice, bob, cara].map(state => expectMessage(state, '* Bob waves')))
  assert(!cara.messages.some(message => message.includes('secret')))
  assert(!alice.messages.includes('[From Alice] secret 内容'))
  assert(!bob.messages.includes('[To Bob] secret 内容'))

  alice.client.chat('/help')
  await waitFor(() => alice.messages.some(message => message.startsWith('/help')), 'private help')
  cara.client.chat('/does_not_exist')
  await expectMessage(cara, 'Unknown command. Use /help.')
  assert(!bob.messages.some(message => message.startsWith('/help') || message.startsWith('Unknown command')))

  settings(cara, 1)
  cara.client.chat('muted outgoing')
  await expectMessage(cara, 'Public chat is disabled in your chat settings.')
  bob.client.chat('public after mode change')
  await expectMessage(alice, '<Bob> public after mode change')
  bob.client.chat('/tell Cara commands only')
  await expectMessage(cara, '[From Bob] commands only')
  assert(!cara.messages.includes('<Bob> public after mode change'))
  assert(!alice.messages.includes('<Cara> muted outgoing'))

  settings(cara, 2)
  cara.client.chat('/list')
  await waitFor(() => cara.messages.filter(m => m.startsWith('Online (3)')).length === 2, 'settings barrier')
  bob.client.chat('/msg Cara hidden')
  await expectMessage(bob, 'Player unavailable.')
  assert(!cara.messages.includes('[From Bob] hidden'))
  settings(cara, 0)
  cara.client.chat('/msg Missing test')
  await expectMessage(cara, 'Player unavailable.')

  // Confirm the chat path stays usable through the first Play heartbeat.
  await new Promise((resolve, reject) => {
    const started = Date.now()
    const timer = setInterval(() => {
      if (alice.heartbeats >= 1 && bob.heartbeats >= 1 && cara.heartbeats >= 1) {
        clearInterval(timer)
        resolve()
      } else if (Date.now() - started > 12000) {
        clearInterval(timer)
        reject(new Error('missing Play heartbeat'))
      }
    }, 50)
  })
  alice.client.chat('/list')
  await expectMessage(alice, 'Online (3): Alice, Bob, Cara')
  console.log('PASS: command tree, async completion, UTF-16 ranges, tooltip, permissions, timeout, Unicode chat, private isolation, and Play heartbeat')
}

run().catch(error => {
  console.error(error)
  process.exitCode = 1
}).finally(() => {
  clearTimeout(deadline)
  for (const client of clients) client.end()
})
