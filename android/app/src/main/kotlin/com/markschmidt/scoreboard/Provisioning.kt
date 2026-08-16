package com.markschmidt.scoreboard

import android.Manifest
import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothManager
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanResult
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.content.ContextCompat
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import uniffi.app_core.UiProvisionStep
import uniffi.app_core.bleContract
import uniffi.app_core.parseNetworks
import uniffi.app_core.provisionSteps
import java.util.UUID

/**
 * BLE Wi-Fi provisioning: hand a brand-new board its network so it can reach
 * the server at all. The board advertises as "Scoreboard Setup" while its
 * flash has no credentials.
 *
 * The CONTRACT — service and characteristic ids, the write sequence, the pin
 * grammar — all comes from the core (`ble_contract`, `provision_steps`),
 * because the firmware refuses a malformed pin and a mis-ordered write, and
 * the only recovery from a bad provisioning is a physical factory reset. A
 * second hand-written copy of those rules is exactly the bug that costs
 * someone their board.
 */
class Provisioner(private val context: Context) {

    sealed interface State {
        data object Idle : State
        data object Searching : State
        data object Connecting : State
        /** Connected; [networks] is what the board scanned before advertising. */
        data class Ready(val networks: List<String>, val canPin: Boolean, val canServer: Boolean) : State
        data object Sending : State
        data object Saved : State
        data class Failed(val why: String) : State
    }

    private val _state = MutableStateFlow<State>(State.Idle)
    val state: StateFlow<State> = _state

    private val contract = bleContract()
    private val serviceId: UUID = UUID.fromString(contract.service)
    private var gatt: BluetoothGatt? = null
    private var pending: MutableList<UiProvisionStep> = mutableListOf()

    private val manager get() = context.getSystemService(BluetoothManager::class.java)

    /** The runtime permissions this flow needs, for the caller to request. */
    fun missingPermissions(): List<String> {
        val needed = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            listOf(Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT)
        } else {
            // Before Android 12, a BLE scan is a location-class capability.
            listOf(Manifest.permission.ACCESS_FINE_LOCATION)
        }
        return needed.filter {
            ContextCompat.checkSelfPermission(context, it) != PackageManager.PERMISSION_GRANTED
        }
    }

    @SuppressLint("MissingPermission")
    fun start() {
        if (missingPermissions().isNotEmpty()) {
            _state.value = State.Failed("Bluetooth permission is needed to set up a scoreboard.")
            return
        }
        val adapter: BluetoothAdapter? = manager?.adapter
        if (adapter == null || !adapter.isEnabled) {
            _state.value = State.Failed("Turn Bluetooth on to set up a scoreboard.")
            return
        }
        _state.value = State.Searching
        // The firmware advertises its NAME but not the 128-bit service UUID,
        // so scan broadly and match on the name — the same reason the iOS
        // client does not filter by service.
        adapter.bluetoothLeScanner?.startScan(scanCallback)
    }

    @SuppressLint("MissingPermission")
    fun stop() {
        runCatching { manager?.adapter?.bluetoothLeScanner?.stopScan(scanCallback) }
        runCatching { gatt?.close() }
        gatt = null
        _state.value = State.Idle
    }

    private val scanCallback = object : ScanCallback() {
        @SuppressLint("MissingPermission")
        override fun onScanResult(callbackType: Int, result: ScanResult) {
            val name = result.device?.name ?: result.scanRecord?.deviceName
            if (name != contract.deviceName) return
            manager?.adapter?.bluetoothLeScanner?.stopScan(this)
            _state.value = State.Connecting
            connect(result.device)
        }

        override fun onScanFailed(errorCode: Int) {
            _state.value = State.Failed("Bluetooth scan failed ($errorCode).")
        }
    }

    @SuppressLint("MissingPermission")
    private fun connect(device: BluetoothDevice) {
        gatt = device.connectGatt(context, false, callback)
    }

    private val callback = object : BluetoothGattCallback() {
        @SuppressLint("MissingPermission")
        override fun onConnectionStateChange(g: BluetoothGatt, status: Int, newState: Int) {
            if (newState == BluetoothGatt.STATE_CONNECTED) {
                g.discoverServices()
            } else if (_state.value == State.Sending) {
                // The board reboots the moment it saves, which drops the link
                // — that disconnect IS the success signal, not a failure.
                _state.value = State.Saved
            }
        }

        @SuppressLint("MissingPermission")
        override fun onServicesDiscovered(g: BluetoothGatt, status: Int) {
            val svc = g.getService(serviceId)
            if (svc == null) {
                _state.value = State.Failed("That scoreboard isn't offering setup.")
                return
            }
            val networks = svc.getCharacteristic(UUID.fromString(contract.networks))
            if (networks != null) {
                g.readCharacteristic(networks)
            } else {
                // Older firmware without the scan list: the user types the SSID.
                _state.value = State.Ready(emptyList(), canPin(svc), canServer(svc))
            }
        }

        @Deprecated("Pre-33 callback; the new one delegates here.")
        override fun onCharacteristicRead(g: BluetoothGatt, c: BluetoothGattCharacteristic, status: Int) {
            @Suppress("DEPRECATION")
            handleRead(g, c.value)
        }

        override fun onCharacteristicRead(
            g: BluetoothGatt,
            c: BluetoothGattCharacteristic,
            value: ByteArray,
            status: Int,
        ) = handleRead(g, value)

        private fun handleRead(g: BluetoothGatt, value: ByteArray?) {
            val svc = g.getService(serviceId) ?: return
            _state.value = State.Ready(
                networks = parseNetworks(value ?: ByteArray(0)),
                canPin = canPin(svc),
                canServer = canServer(svc),
            )
        }

        @SuppressLint("MissingPermission")
        override fun onCharacteristicWrite(g: BluetoothGatt, c: BluetoothGattCharacteristic, status: Int) {
            if (status != BluetoothGatt.GATT_SUCCESS) {
                // A refused write is the firmware rejecting the VALUE (a
                // malformed pin is refused at the ATT layer rather than
                // stored) — stop instead of sending the password, which would
                // trigger a save the board then refuses silently.
                _state.value = State.Failed("The scoreboard refused that value. Check the server pin.")
                return
            }
            writeNext(g)
        }

        private fun canPin(svc: android.bluetooth.BluetoothGattService) =
            svc.getCharacteristic(UUID.fromString(contract.pin)) != null

        private fun canServer(svc: android.bluetooth.BluetoothGattService) =
            svc.getCharacteristic(UUID.fromString(contract.server)) != null
    }

    /**
     * Send credentials. The step list — including the rule that the password
     * goes LAST because it triggers the board's save-and-reboot — is the
     * core's.
     */
    @SuppressLint("MissingPermission")
    fun provision(ssid: String, password: String, wsUrl: String, pinRaw: String) {
        val g = gatt ?: run {
            _state.value = State.Failed("Not connected to a scoreboard.")
            return
        }
        val ready = _state.value as? State.Ready ?: return
        val steps = try {
            provisionSteps(ssid, password, wsUrl, pinRaw, ready.canPin, ready.canServer)
        } catch (e: Exception) {
            _state.value = State.Failed(e.message ?: "Couldn't build the setup sequence.")
            return
        }
        pending = steps.toMutableList()
        _state.value = State.Sending
        writeNext(g)
    }

    @SuppressLint("MissingPermission")
    private fun writeNext(g: BluetoothGatt) {
        val step = pending.removeFirstOrNull() ?: return
        val svc = g.getService(serviceId) ?: return
        val c = svc.getCharacteristic(UUID.fromString(step.uuid)) ?: return
        val bytes = step.value.toByteArray()
        // withResponse throughout: each ack drives the next write, which is
        // what keeps the ORDER the firmware requires.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            g.writeCharacteristic(c, bytes, BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT)
        } else {
            @Suppress("DEPRECATION")
            run {
                c.value = bytes
                c.writeType = BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT
                g.writeCharacteristic(c)
            }
        }
    }
}
