udx_protocol = Proto("UDX",  "UDX Protocol")
udx_protocol.fields.version = ProtoField.uint8("udx.version", "Version", base.DEC)
udx_protocol.fields.type = ProtoField.uint8("udx.type", "Type", base.DEC)
udx_protocol.fields.id = ProtoField.uint32("udx.id", "Stream", base.DEC)
udx_protocol.fields.seq = ProtoField.uint32("udx.seq", "Seq", base.DEC)
udx_protocol.fields.ack = ProtoField.uint32("udx.ack", "Ack", base.DEC)
udx_protocol.fields.length = ProtoField.uint32("udx.length", "Length", base.DEC)
udx_protocol.fields.data = ProtoField.protocol("udx.data", "UDX payload")

local TYPE_DATA = 1
local TYPE_END = 2
local TYPE_SACK = 4
local TYPE_MSG = 8
local TYPE_DESTROY = 16

local function get_type_name(type)
    local txt = "STATE,"

    if bit.band(type, TYPE_DATA) == 1 then txt = txt .. "DATA," end
    if bit.band(type, TYPE_END) == 1 then txt = txt .. "END," end
    if bit.band(type, TYPE_SACK) == 1 then txt = txt .. "SACK," end
    if bit.band(type, TYPE_MSG) == 1 then txt = txt .. "MSG," end
    if bit.band(type, TYPE_DESTROY) == 1 then txt = txt .. "DESTROY," end

    return txt:sub(1,-2)
end


function udx_protocol.dissector(buffer, pinfo, tree)
  length = buffer:len()
  if length == 0 then return end

  pinfo.cols.protocol = udx_protocol.name

  local subtree = tree:add(udx_protocol, buffer(), "UDX Protocol")

  local typ = buffer(2,1):le_int()
  local type_names = get_type_name(typ)

  local id = buffer(4,4):le_int()
  local seq = buffer(12,4):le_int()
  local ack = buffer(16,4):le_int()
  local data_len = length - 20

  subtree:add_le(udx_protocol.fields.version, buffer(1,1))
  subtree:add_le(udx_protocol.fields.type, buffer(2,1)):append_text(" (" .. type_names .. ")")
  subtree:add_le(udx_protocol.fields.id, buffer(4,4))
  subtree:add_le(udx_protocol.fields.seq, buffer(12,4))
  subtree:add_le(udx_protocol.fields.ack, buffer(16,4))
  subtree:add(udx_protocol.fields.length, data_len):set_generated(true)
  subtree:add(udx_protocol.fields.data, buffer(20,data_len)):append_text(" (" .. data_len .. " bytes)")

  local info = pinfo.src_port .. " → " .. pinfo.dst_port .. " Stream=" .. id .. " Seq=" .. seq .. " Ack=" .. ack .. " (" .. type_names .. ")"
  info = info .. " Len=" .. data_len
  if bit.band(typ, TYPE_END) == 1 then
    info = info .. " END"
  end
  if bit.band(typ, TYPE_END) == 1 then
    info = info .. " END"
  end
  pinfo.cols.info:set(info)
end


-- heuristic_checker: determine which dissector to use
local function heuristic_checker(buffer, pinfo, tree)
    local magic_byte = buffer(0,1):le_uint()
    local version = buffer(1,1):le_uint()
    local typ = buffer(2,1):le_uint()
    if magic_byte == 255 and version == 1 and typ < 32 then
        udx_protocol.dissector(buffer, pinfo, tree)
        return true
    else
      return false
    end
end

udx_protocol:register_heuristic('udp', heuristic_checker)
