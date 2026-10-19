@file:JvmName("Sample")

package com.example.sample

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.*
import java.util.concurrent.atomic.AtomicInteger as Counter

/**
 * A repository of [User]s.
 */
interface Repository<T : Any, ID> {
    suspend fun find(id: ID): T?
    fun all(): Flow<T>
}

data class User(
    val id: Long,
    val name: String,
    val email: String? = null,
)

class InMemoryRepository(
    private val scope: CoroutineScope,
) : Repository<User, Long> {
    private val users = mutableMapOf<Long, User>()
    private val counter = Counter(0)

    override suspend fun find(id: Long): User? = withContext(Dispatchers.IO) {
        users[id]
    }

    override fun all(): Flow<User> = flow {
        for ((_, user) in users) {
            emit(user)
        }
    }

    fun add(name: String, email: String? = null): User {
        val id = counter.incrementAndGet().toLong()
        val user = User(id, name, email)
        users[id] = user
        return user
    }

    fun describe(user: User): String = when {
        user.email == null -> "${user.name} (no email)"
        user.email.endsWith("@example.com") -> "${user.name} <internal>"
        else -> "${user.name} <${user.email}>"
    }

    companion object {
        private const val TAG = "Repo"

        @JvmStatic
        fun create(scope: CoroutineScope = GlobalScope): InMemoryRepository = InMemoryRepository(scope)
    }
}

sealed class Result<out T> {
    data class Ok<T>(val value: T) : Result<T>()
    data class Err(val error: Throwable) : Result<Nothing>()

    inline fun <R> map(transform: (T) -> R): Result<R> = when (this) {
        is Ok -> Ok(transform(value))
        is Err -> this
    }
}

fun <T> Result<T>.getOrNull(): T? = (this as? Result.Ok)?.value

val Int.isEven: Boolean
    get() = this % 2 == 0

object Registry {
    private val handlers = HashMap<String, (String) -> Unit>()

    fun register(name: String, handler: (String) -> Unit) {
        handlers[name] = handler
    }

    operator fun get(name: String) = handlers[name]
}

fun main(args: Array<String>) = runBlocking {
    val repo = InMemoryRepository.create(this)
    repo.add("alice", "alice@example.com")
    repo.add("bob")
    repo.all().collect { println(repo.describe(it)) }
    val evens = (1..10).filter { it.isEven }.map { n -> n * n }
    println("Evens: $evens, args=${args.joinToString()}")
    Registry.register("echo") { println(it) }
    Registry["echo"]?.invoke("hi")
    val job = launch {
        repeat(3) { i ->
            delay(10L * i)
        }
    }
    job.join()
}
