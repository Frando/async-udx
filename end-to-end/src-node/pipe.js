import UDX from 'udx-native'
// import why from 'why-is-node-running'
// import silly from 'sillyname'

const LOCALHOST = '127.0.0.1'

main().catch(err => {
  console.error(err)
  process.exit(1)
}).then(() => {
  console.log('done')
  process.exit(0)
})

async function main () {
  // const mode = process.env.MODE
  const udx = new UDX()
  const socket = udx.createSocket()
  socket.bind()
  const localPort = socket.address().port
  console.log(localPort)
  console.error(`listening on localhost:${localPort}`)

  const port = await readPort()
  const stream = udx.createStream(1)
  const host = LOCALHOST
  console.error(`connect to ${host}:${port}`)
  stream.connect(socket, 1, port, host)
  stream.on('error', err => {
    console.error(`${err.code} ${err.message}`)
    stream.destroy(err)
  })
  stream.on('end', () => {
    console.error('end')
    stream.destroy()
  })
  run(stream)
  await new Promise((resolve, reject) => {
    stream.on('close', resolve)
    stream.once('error', err => err.code === 'ECONNRESET' ? resolve() : reject(err))
  })
  socket.close()
}

function run (stream) {
  stream.on('data', data => {
    console.error('[recv]', data.toString())
    // stream.end()
  })
  for (let i = 0; i < 10; i++) {
    const message = `hello from node ${i}`
    stream.write(message + '\n')
    console.error('[send]', message)
  }
  stream.on('end', () => {
    console.error('RECV END')
  })
  stream.on('close', () => {
    console.error('RECV CLOSE')
  })
}

async function readPort () {
  return new Promise((resolve, reject) => {
    process.stdin.once('data', data => {
      const line = data.toString().split('\n')[0]
      const number = Number(line)
      if (!number) reject(new Error(`Expected port, got ${line}`))
      else resolve(number)
    })
  })
}
